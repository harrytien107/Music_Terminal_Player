use std::sync::mpsc::{self, Receiver};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MediaCommand {
    Play,
    Pause,
    Next,
    Previous,
}

pub(crate) struct MediaControls {
    commands: Receiver<MediaCommand>,
    #[cfg(windows)]
    inner: Option<windows_impl::WindowsMediaControls>,
}

impl MediaControls {
    pub(crate) fn new() -> Self {
        let (sender, commands) = mpsc::channel();
        #[cfg(windows)]
        let inner = windows_impl::WindowsMediaControls::new(sender).ok();
        #[cfg(not(windows))]
        drop(sender);
        Self {
            commands,
            #[cfg(windows)]
            inner,
        }
    }

    pub(crate) fn command(&self) -> Option<MediaCommand> {
        self.commands.try_recv().ok()
    }

    pub(crate) fn set_track(&self, title: &str, artist: &str) {
        #[cfg(windows)]
        if let Some(inner) = &self.inner {
            let _ = inner.set_track(title, artist);
        }
        #[cfg(not(windows))]
        let _ = (title, artist);
    }

    pub(crate) fn set_playing(&self, playing: bool) {
        #[cfg(windows)]
        if let Some(inner) = &self.inner {
            let _ = inner.set_playing(playing);
        }
        #[cfg(not(windows))]
        let _ = playing;
    }
}

#[cfg(windows)]
mod windows_impl {
    use std::sync::mpsc::Sender;

    use windows::Foundation::TypedEventHandler;
    use windows::Media::{
        MediaPlaybackStatus, MediaPlaybackType, SystemMediaTransportControls,
        SystemMediaTransportControlsButton, SystemMediaTransportControlsButtonPressedEventArgs,
    };
    use windows::Win32::System::Console::GetConsoleWindow;
    use windows::Win32::System::WinRT::{
        ISystemMediaTransportControlsInterop, RO_INIT_MULTITHREADED, RoInitialize,
    };
    use windows::core::{HSTRING, Result, factory};

    use super::MediaCommand;

    fn media_command(button: SystemMediaTransportControlsButton) -> Option<MediaCommand> {
        match button {
            SystemMediaTransportControlsButton::Play => Some(MediaCommand::Play),
            SystemMediaTransportControlsButton::Pause => Some(MediaCommand::Pause),
            SystemMediaTransportControlsButton::Next => Some(MediaCommand::Next),
            SystemMediaTransportControlsButton::Previous => Some(MediaCommand::Previous),
            _ => None,
        }
    }

    pub(super) struct WindowsMediaControls {
        controls: SystemMediaTransportControls,
        button_token: i64,
    }

    impl WindowsMediaControls {
        pub(super) fn new(sender: Sender<MediaCommand>) -> Result<Self> {
            // SAFETY: Initializing WinRT more than once on the same thread is supported.
            let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
            // SAFETY: GetConsoleWindow returns either the current console HWND or null.
            let window = unsafe { GetConsoleWindow() };
            if window.0.is_null() {
                return Err(windows::core::Error::empty());
            }
            let interop =
                factory::<SystemMediaTransportControls, ISystemMediaTransportControlsInterop>()?;
            // SAFETY: The HWND is owned by the current console session and the requested type is SMTC.
            let controls: SystemMediaTransportControls = unsafe { interop.GetForWindow(window)? };
            controls.SetIsPlayEnabled(true)?;
            controls.SetIsPauseEnabled(true)?;
            controls.SetIsNextEnabled(true)?;
            controls.SetIsPreviousEnabled(true)?;
            controls.SetIsEnabled(true)?;

            let handler = TypedEventHandler::<
                SystemMediaTransportControls,
                SystemMediaTransportControlsButtonPressedEventArgs,
            >::new(move |_controls, args| {
                if let Some(command) = media_command(args.ok()?.Button()?) {
                    let _ = sender.send(command);
                }
                Ok(())
            });
            let button_token = controls.ButtonPressed(&handler)?;
            Ok(Self {
                controls,
                button_token,
            })
        }

        pub(super) fn set_track(&self, title: &str, artist: &str) -> Result<()> {
            let updater = self.controls.DisplayUpdater()?;
            updater.SetType(MediaPlaybackType::Music)?;
            let properties = updater.MusicProperties()?;
            properties.SetTitle(&HSTRING::from(title))?;
            properties.SetArtist(&HSTRING::from(artist))?;
            updater.Update()
        }

        pub(super) fn set_playing(&self, playing: bool) -> Result<()> {
            self.controls.SetPlaybackStatus(if playing {
                MediaPlaybackStatus::Playing
            } else {
                MediaPlaybackStatus::Paused
            })
        }
    }

    impl Drop for WindowsMediaControls {
        fn drop(&mut self) {
            let _ = self.controls.RemoveButtonPressed(self.button_token);
            let _ = self
                .controls
                .SetPlaybackStatus(MediaPlaybackStatus::Stopped);
            let _ = self.controls.SetIsEnabled(false);
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn transport_buttons_map_to_player_commands() {
            assert_eq!(
                media_command(SystemMediaTransportControlsButton::Play),
                Some(MediaCommand::Play)
            );
            assert_eq!(
                media_command(SystemMediaTransportControlsButton::Pause),
                Some(MediaCommand::Pause)
            );
            assert_eq!(
                media_command(SystemMediaTransportControlsButton::Next),
                Some(MediaCommand::Next)
            );
            assert_eq!(
                media_command(SystemMediaTransportControlsButton::Previous),
                Some(MediaCommand::Previous)
            );
            assert_eq!(
                media_command(SystemMediaTransportControlsButton::Stop),
                None
            );
        }
    }
}
