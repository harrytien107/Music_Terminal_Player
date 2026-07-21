use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::app::{Playlist, parse_playlists, serialize_playlists};
use crate::local::search_local_tracks;
use crate::telegram::{
    TelegramCatalogEntry, checked_position, delete_cache_directory, load_telegram_catalog,
    save_telegram_catalog, search_catalog,
};
use crate::util::{
    LoopMode, fit_text, insert_queue_next, is_supported_audio_path, normalize_channel,
    parse_volume_settings, playback_controls, progress_bar, remove_queue_item, safe_file_name,
    shuffle_slice, toggle_all,
};
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
fn playlist_round_trips_channel_tracks_and_order() {
    let playlist = Playlist {
        name: "Road\tTrip".to_string(),
        channel: "music_channel".to_string(),
        tracks: vec![
            TelegramCatalogEntry {
                message_id: 7,
                name: "First song.mp3".to_string(),
            },
            TelegramCatalogEntry {
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
    assert_eq!(parsed[0].tracks[0].message_id, 7);
    assert_eq!(parsed[0].tracks[1].name, "Second song.flac");
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
fn telegram_catalog_round_trips_track_ids_and_names() {
    let path = PathBuf::from("target/test-telegram-catalog.txt");
    let catalog = vec![
        TelegramCatalogEntry {
            message_id: 12,
            name: "First song.mp3".to_string(),
        },
        TelegramCatalogEntry {
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
            message_id: 1,
            name: "First Song.mp3".to_string(),
        },
        TelegramCatalogEntry {
            message_id: 2,
            name: "Another track.flac".to_string(),
        },
        TelegramCatalogEntry {
            message_id: 3,
            name: "Second SONG.ogg".to_string(),
        },
    ];

    assert_eq!(search_catalog(&catalog, " song "), vec![0, 2]);
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
