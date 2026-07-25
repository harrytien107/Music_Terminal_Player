use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::app::{Playlist, collect_library_tracks, parse_playlists, serialize_playlists};
use crate::local::search_local_tracks;
use crate::telegram::{
    TelegramCatalogEntry, checked_position, delete_cache_directory, is_opus_path,
    load_telegram_catalog, normalize_channel_identity, private_channel_identity,
    private_channel_is_saved, private_invite_hash, save_telegram_catalog, search_catalog,
    search_private_channels, selectable_private_channel_matches, telegram_channel_label,
};
use crate::util::{
    LoopMode, fit_text, insert_queue_next, is_supported_audio_path, normalize_channel,
    parse_volume_settings, playback_controls, progress_bar, queue_window_start, remove_queue_item,
    safe_file_name, shuffle_slice, toggle_all,
};
use crate::youtube::{
    YouTubeTools, parse_youtube_tools, parse_youtube_urls, serialize_youtube_tools,
};
use grammers_session::types::{PeerAuth, PeerId, PeerRef};

#[test]
fn safe_file_name_removes_windows_forbidden_chars() {
    assert_eq!(
        safe_file_name("a<b>:c\"d/e\\f|g?h*.mp3"),
        "a_b__c_d_e_f_g_h_.mp3"
    );
    assert_eq!(safe_file_name("..."), "track.bin");
}

#[test]
fn supported_audio_extensions_are_case_insensitive() {
    assert!(is_supported_audio_path(Path::new("Song.MP3")));
    assert!(!is_supported_audio_path(Path::new("cover.png")));
}

#[test]
fn telegram_opus_backend_matches_extension_case_insensitively() {
    assert!(is_opus_path(Path::new("voice.OPUS")));
    assert!(!is_opus_path(Path::new("voice.ogg")));
}

#[test]
fn checked_position_rejects_negative_seek() {
    assert_eq!(checked_position(10, -4).unwrap(), 6);
    assert!(checked_position(3, -4).is_err());
}

#[test]
fn saved_channel_input_accepts_username_and_url() {
    assert_eq!(normalize_channel("@music_channel"), "music_channel");
    assert_eq!(
        normalize_channel("https://t.me/music_channel/"),
        "music_channel"
    );
}

#[test]
fn private_channel_identity_round_trips_through_library_and_playlist_formats() {
    let channel = private_channel_identity(
        PeerRef {
            id: PeerId::channel(123456).unwrap(),
            auth: PeerAuth::from_hash(987654321),
        },
        "My private 音楽",
    );
    assert_eq!(normalize_channel_identity(&channel), channel);
    assert_eq!(telegram_channel_label(&channel), "🔒 My private 音楽");

    let playlist = Playlist {
        name: "Private mix".to_string(),
        channel: channel.clone(),
        tracks: vec![TelegramCatalogEntry {
            channel: channel.clone(),
            message_id: 42,
            name: "Private song.opus".to_string(),
        }],
        local_tracks: Vec::new(),
    };
    let parsed = parse_playlists(&serialize_playlists(&[playlist]));
    assert_eq!(parsed[0].channel, channel);
    assert_eq!(parsed[0].tracks[0].channel, channel);
}

#[test]
fn private_saved_marker_matches_peer_id_after_title_changes() {
    let peer = PeerRef {
        id: PeerId::channel(123456).unwrap(),
        auth: PeerAuth::from_hash(987654321),
    };
    let saved = private_channel_identity(peer, "Old title");
    let discovered = private_channel_identity(peer, "Renamed channel");
    let other = private_channel_identity(
        PeerRef {
            id: PeerId::channel(654321).unwrap(),
            auth: PeerAuth::from_hash(123456789),
        },
        "Other channel",
    );
    assert!(private_channel_is_saved(&discovered, &[saved]));
    assert!(!private_channel_is_saved(&other, &[]));
}

#[test]
fn saved_private_channels_are_excluded_from_bulk_selection() {
    let saved = HashSet::from([1, 3]);
    assert_eq!(
        selectable_private_channel_matches(&[0, 1, 2, 3], &saved),
        vec![0, 2]
    );
}

#[test]
fn private_invite_links_extract_hash_without_accepting_the_invite() {
    assert_eq!(
        private_invite_hash("https://t.me/+AbCd_123"),
        Some("AbCd_123".to_string())
    );
    assert_eq!(
        private_invite_hash("https://t.me/joinchat/AbCd_123/"),
        Some("AbCd_123".to_string())
    );
    assert_eq!(private_invite_hash("https://t.me/public_channel"), None);
}

#[test]
fn private_channel_search_matches_title_and_channel_id() {
    let channels = vec![
        ("AdSecVN Bak".to_string(), 3714482295, "first".to_string()),
        ("Music Vault".to_string(), 123456, "second".to_string()),
    ];
    assert_eq!(search_private_channels(&channels, "adsec"), vec![0]);
    assert_eq!(search_private_channels(&channels, "123456"), vec![1]);
    assert_eq!(search_private_channels(&channels, ""), vec![0, 1]);
}

#[test]
fn youtube_input_accepts_videos_playlists_and_multiple_urls() {
    let urls = parse_youtube_urls("https://youtu.be/abc https://www.youtube.com/playlist?list=xyz")
        .unwrap();
    assert_eq!(urls.len(), 2);
    assert!(parse_youtube_urls("https://example.com/video").is_err());
}

#[test]
fn youtube_tool_paths_round_trip_without_touching_volume_settings() {
    let tools = YouTubeTools {
        yt_dlp: r"D:\Portable Apps\yt-dlp.exe".to_string(),
        ffmpeg: r"D:\Portable Apps\ffmpeg.exe".to_string(),
    };
    assert_eq!(
        parse_youtube_tools(&serialize_youtube_tools(&tools)),
        Some(tools)
    );
    assert!(parse_youtube_tools("yt_dlp=yt-dlp\n").is_none());
}

#[test]
fn wide_song_names_fit_inside_terminal_cells() {
    let fitted = fit_text("巨大なものが来る song title", 12);
    assert_eq!(unicode_width::UnicodeWidthStr::width(fitted.as_str()), 12);
    assert!(fitted.trim_end().ends_with('…'));
}

#[test]
fn queue_window_follows_selected_track() {
    assert_eq!(queue_window_start(27, 10, 40, 12), 11);
    assert_eq!(queue_window_start(39, 10, 40, 12), 18);
    assert_eq!(queue_window_start(4, 10, 40, 12), 0);
}

#[test]
fn player_progress_bar_is_bounded() {
    use std::time::Duration;

    assert_eq!(
        progress_bar(Duration::from_secs(30), Some(Duration::from_secs(60)), 10),
        "/////     "
    );
    assert_eq!(
        progress_bar(Duration::from_secs(90), Some(Duration::from_secs(60)), 10),
        "//////////"
    );
}

#[test]
fn panel_text_is_padded_or_clipped() {
    assert_eq!(fit_text("song", 6), "song  ");
    assert_eq!(fit_text("long song", 6), "long …");
}

#[test]
fn playback_control_table_has_aligned_rows() {
    let rows = playback_controls();
    let width = rows[0].chars().count();
    assert!(rows.iter().all(|row| row.chars().count() == width));
    assert!(rows.join("\n").contains("[p] play/pause"));
    assert!(!rows.join("\n").contains("lyrics"));
}

#[test]
fn loop_mode_cycles_off_all_one() {
    assert!(LoopMode::Off.cycle() == LoopMode::All);
    assert!(LoopMode::All.cycle() == LoopMode::One);
    assert!(LoopMode::One.cycle() == LoopMode::Off);
}

#[test]
fn saved_volume_defaults_and_stays_bounded() {
    assert_eq!(parse_volume_settings("volume=1.25\n"), 1.25);
    assert_eq!(parse_volume_settings("volume=9\n"), 1.5);
    assert_eq!(parse_volume_settings("invalid\n"), 0.8);
}

#[test]
fn playlist_round_trips_multi_channel_tracks_and_order() {
    let playlist = Playlist {
        name: "Road\tTrip".to_string(),
        channel: "music_channel".to_string(),
        tracks: vec![
            TelegramCatalogEntry {
                channel: "music_channel".to_string(),
                message_id: 7,
                name: "First song.mp3".to_string(),
            },
            TelegramCatalogEntry {
                channel: "other_channel".to_string(),
                message_id: 9,
                name: "Second song.flac".to_string(),
            },
        ],
        local_tracks: Vec::new(),
    };
    let parsed = parse_playlists(&serialize_playlists(&[playlist]));

    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].name, "Road Trip");
    assert_eq!(parsed[0].channel, "music_channel");
    assert_eq!(parsed[0].tracks[0].channel, "music_channel");
    assert_eq!(parsed[0].tracks[0].message_id, 7);
    assert_eq!(parsed[0].tracks[1].channel, "other_channel");
    assert_eq!(parsed[0].tracks[1].name, "Second song.flac");
}

#[test]
fn old_single_channel_playlist_assigns_channel_to_tracks() {
    let parsed =
        parse_playlists("playlist\tLegacy mix\tlegacy_channel\ntrack\t42\tLegacy song.mp3\n");

    assert_eq!(parsed[0].tracks[0].channel, "legacy_channel");
    assert_eq!(parsed[0].tracks[0].message_id, 42);
    assert_eq!(parsed[0].tracks[0].name, "Legacy song.mp3");
}

#[test]
fn local_playlist_round_trips_paths_and_searches() {
    let playlist = Playlist {
        name: "Local mix".to_string(),
        channel: String::new(),
        tracks: Vec::new(),
        local_tracks: vec![
            PathBuf::from(r"D:\\Music\\First Song.mp3"),
            PathBuf::from(r"D:\\Music\\Second.flac"),
        ],
    };
    let parsed = parse_playlists(&serialize_playlists(&[playlist]));

    assert_eq!(parsed[0].local_tracks.len(), 2);
    assert_eq!(
        parsed[0].local_tracks[1],
        PathBuf::from(r"D:\\Music\\Second.flac")
    );
    assert_eq!(
        search_local_tracks(&parsed[0].local_tracks, "first"),
        vec![0]
    );
}

#[test]
fn toggle_all_selects_then_deselects_only_matches() {
    let mut selected = HashSet::from([9]);
    toggle_all(&mut selected, [1, 2]);
    assert_eq!(selected, HashSet::from([1, 2, 9]));

    toggle_all(&mut selected, [1, 2]);
    assert_eq!(selected, HashSet::from([9]));
}

#[test]
fn removing_queue_items_keeps_current_track_index() {
    let mut queue = vec!["a", "b", "c", "d"];
    let current = remove_queue_item(&mut queue, 2, 0);
    assert_eq!(queue, vec!["b", "c", "d"]);
    assert_eq!(current, 1);

    let current = remove_queue_item(&mut queue, current, current);
    assert_eq!(queue, vec!["b", "d"]);
    assert_eq!(current, 1);
}

#[test]
fn added_queue_songs_play_next_then_resume_original_list() {
    let mut queue = vec!["a", "b", "c", "d"];
    let mut play_next = 0;
    insert_queue_next(&mut queue, 1, &mut play_next, vec!["x", "y"]);
    insert_queue_next(&mut queue, 1, &mut play_next, vec!["z"]);
    assert_eq!(queue, vec!["a", "b", "x", "y", "z", "c", "d"]);
    assert_eq!(play_next, 3);
}

#[test]
fn shuffle_preserves_every_item() {
    let mut items = vec![1, 2, 3, 4, 5];
    shuffle_slice(&mut items);
    items.sort_unstable();
    assert_eq!(items, vec![1, 2, 3, 4, 5]);
}

#[test]
fn local_library_collection_combines_folders_without_duplicates() {
    let root = PathBuf::from("target/test-quick-play-local-library");
    let nested = root.join("album");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&nested).unwrap();
    fs::write(root.join("first.mp3"), b"").unwrap();
    fs::write(nested.join("second.flac"), b"").unwrap();
    fs::write(nested.join("cover.png"), b"").unwrap();

    let tracks = collect_library_tracks(&[root.clone(), nested]).unwrap();
    fs::remove_dir_all(root).unwrap();

    assert_eq!(tracks.len(), 2);
    assert!(tracks.iter().any(|track| track.ends_with("first.mp3")));
    assert!(tracks.iter().any(|track| track.ends_with("second.flac")));
}

#[test]
fn telegram_catalog_round_trips_track_ids_and_names() {
    let path = PathBuf::from("target/test-telegram-catalog.txt");
    let catalog = vec![
        TelegramCatalogEntry {
            channel: String::new(),
            message_id: 12,
            name: "First song.mp3".to_string(),
        },
        TelegramCatalogEntry {
            channel: String::new(),
            message_id: 34,
            name: "Second\ttrack\n.flac".to_string(),
        },
    ];

    save_telegram_catalog(&path, &catalog).unwrap();
    let loaded = load_telegram_catalog(&path).unwrap();
    fs::remove_file(path).unwrap();

    assert_eq!(loaded[0], catalog[0]);
    assert_eq!(loaded[1].message_id, 34);
    assert_eq!(loaded[1].name, "Second track .flac");
}

#[test]
fn telegram_catalog_search_is_case_insensitive_and_keeps_original_indexes() {
    let catalog = vec![
        TelegramCatalogEntry {
            channel: "first_channel".to_string(),
            message_id: 1,
            name: "First Song.mp3".to_string(),
        },
        TelegramCatalogEntry {
            channel: "other_channel".to_string(),
            message_id: 2,
            name: "Another track.flac".to_string(),
        },
        TelegramCatalogEntry {
            channel: "second_channel".to_string(),
            message_id: 3,
            name: "Second SONG.ogg".to_string(),
        },
    ];

    assert_eq!(search_catalog(&catalog, " song "), vec![0, 2]);
    assert_eq!(search_catalog(&catalog, "other_channel"), vec![1]);
    assert_eq!(search_catalog(&catalog, "missing"), Vec::<usize>::new());
}

#[test]
fn delete_cache_directory_removes_every_file_and_ignores_missing_directory() {
    let cache = PathBuf::from("target/test-telegram-cache-cleanup");
    let nested = cache.join("old-channel");
    let _ = fs::remove_dir_all(&cache);
    fs::create_dir_all(&nested).unwrap();
    fs::write(cache.join("complete.mp3"), b"complete").unwrap();
    fs::write(nested.join("partial.mp3"), b"partial").unwrap();

    delete_cache_directory(&cache).unwrap();
    assert!(!cache.exists());
    delete_cache_directory(&cache).unwrap();
}
