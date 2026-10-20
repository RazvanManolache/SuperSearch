use super::*;
use std::path::PathBuf;

fn sample_config() -> AppConfig {
    AppConfig {
        roots: vec![PathBuf::from("/tmp/docs"), PathBuf::from("/tmp/pics")],
        db_path: Some(PathBuf::from("/tmp/index.db")),
        window: WindowState {
            width: 1280.0,
            height: 800.0,
            maximized: false,
        },
        saved_searches: vec![SavedSearch {
            name: "reports".to_string(),
            name_filter: Some("report".to_string()),
            extensions: vec!["md".to_string()],
            case_insensitive: true,
            ..SavedSearch::default()
        }],
        recents: vec![RecentEntry::Legacy("report".to_string())],
        exclusions: vec!["target".to_string(), "*.tmp".to_string()],
        presets: vec![Preset {
            name: "code".to_string(),
            extensions: vec!["rs".to_string(), "toml".to_string()],
            exclusions: vec!["target".to_string()],
        }],
        file_groups: vec![FileGroup {
            name: "Docs".to_string(),
            extensions: vec!["md".to_string()],
        }],
        csv_delim: ';',
        language: "en".to_string(),
        ui_scale: 1.0,
    }
}

#[test]
fn default_is_empty_with_sane_window() {
    let cfg = AppConfig::default();
    assert!(cfg.roots.is_empty());
    assert!(cfg.db_path.is_none());
    assert_eq!(cfg.window.width, 1024.0);
    assert_eq!(cfg.window.height, 768.0);
    assert!(!cfg.window.maximized);
    assert!(cfg.saved_searches.is_empty());
    assert!(cfg.recents.is_empty());
    assert!(cfg.exclusions.is_empty());
    assert!(cfg.presets.is_empty());
    assert!(cfg.file_groups.is_empty());
}

#[test]
fn json_roundtrip_preserves_every_field() {
    let cfg = sample_config();
    let text = serde_json::to_string(&cfg).expect("serialize");
    let back: AppConfig = serde_json::from_str(&text).expect("deserialize");
    assert_eq!(cfg, back);
}

#[test]
fn pretty_json_mentions_all_top_level_keys() {
    let cfg = sample_config();
    let text = serde_json::to_string_pretty(&cfg).expect("serialize");
    for key in [
        "roots",
        "db_path",
        "window",
        "saved_searches",
        "recents",
        "exclusions",
        "presets",
        "file_groups",
        "csv_delim",
        "ui_scale",
    ] {
        assert!(text.contains(&format!("\"{key}\"")), "missing key {key}");
    }
}

#[test]
fn missing_file_loads_default() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("no-such-dir").join("config.json");
    let cfg = AppConfig::load_from(&path).expect("load");
    assert_eq!(cfg, AppConfig::default());
}

#[test]
fn save_then_load_roundtrips_through_disk() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nested").join("config.json");
    let cfg = sample_config();
    cfg.save_to(&path).expect("save");
    let back = AppConfig::load_from(&path).expect("load");
    assert_eq!(cfg, back);
}

#[test]
fn corrupt_file_is_an_error_not_a_panic() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.json");
    fs::write(&path, "{ not valid json").expect("write");
    match AppConfig::load_from(&path) {
        Err(ConfigError::Json(_)) => {}
        other => panic!("expected JSON error, got {other:?}"),
    }
}

#[test]
fn empty_file_loads_default() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.json");
    fs::write(&path, "   \n").expect("write");
    let cfg = AppConfig::load_from(&path).expect("load");
    assert_eq!(cfg, AppConfig::default());
}

#[test]
fn push_recent_dedupes_moves_to_front_and_caps() {
    let mut cfg = AppConfig::default();
    cfg.push_recent("  ");
    assert!(cfg.recents.is_empty());
    cfg.push_recent("a");
    cfg.push_recent("b");
    cfg.push_recent("a");
    assert_eq!(
        cfg.recents,
        vec![
            RecentEntry::Legacy("a".to_string()),
            RecentEntry::Legacy("b".to_string())
        ]
    );
    for i in 0..(MAX_RECENTS + 10) {
        cfg.push_recent(format!("q{i}"));
    }
    assert_eq!(cfg.recents.len(), MAX_RECENTS);
    assert_eq!(
        cfg.recents[0],
        RecentEntry::Legacy(format!("q{}", MAX_RECENTS + 9))
    );
}

#[test]
fn saved_search_add_replaces_and_remove_works() {
    let mut cfg = AppConfig::default();
    cfg.add_saved_search(SavedSearch {
        name: "a".into(),
        name_filter: Some("x".into()),
        ..SavedSearch::default()
    });
    cfg.add_saved_search(SavedSearch {
        name: "b".into(),
        ..SavedSearch::default()
    });
    cfg.add_saved_search(SavedSearch {
        name: "a".into(),
        name_filter: Some("y".into()),
        ..SavedSearch::default()
    });
    assert_eq!(cfg.saved_searches.len(), 2);
    let a = cfg.saved_searches.iter().find(|s| s.name == "a").unwrap();
    assert_eq!(a.name_filter.as_deref(), Some("y"));
    assert!(cfg.remove_saved_search("a"));
    assert!(!cfg.remove_saved_search("a"));
    assert_eq!(cfg.saved_searches.len(), 1);
}

#[test]
#[test]
fn file_groups_resolve_customs_then_builtins() {
    let cfg = AppConfig::default();
    let images = cfg.resolve_file_group("images").expect("builtin");
    assert!(images.contains(&"png".to_string()));
    assert!(cfg.resolve_file_group("nope").is_none());
    let mut custom = AppConfig::default();
    custom.file_groups.push(FileGroup {
        name: "Images".to_string(),
        extensions: vec!["exr".to_string()],
    });
    assert_eq!(
        custom.resolve_file_group("IMAGES"),
        Some(vec!["exr".to_string()])
    );
    let all = custom.all_file_groups();
    assert!(all
        .iter()
        .any(|g| g.name == "Images" && g.extensions == vec!["exr"]));
    assert!(all.iter().any(|g| g.name == "Documents"));
    assert_eq!(all.iter().filter(|g| g.name == "Images").count(), 1);
}

#[test]
fn preset_upsert_replaces_same_name() {
    let mut cfg = AppConfig::default();
    cfg.upsert_preset(Preset {
        name: "p".into(),
        extensions: vec!["rs".into()],
        exclusions: vec![],
    });
    cfg.upsert_preset(Preset {
        name: "p".into(),
        extensions: vec!["md".into()],
        exclusions: vec!["x".into()],
    });
    assert_eq!(cfg.presets.len(), 1);
    assert_eq!(cfg.presets[0].extensions, vec!["md".to_string()]);
}

#[test]
fn saved_search_query_roundtrip() {
    let saved = SavedSearch {
        name: "s".into(),
        name_filter: Some("rep".into()),
        regex: Some("a.*b".into()),
        fuzzy: Some("mrs".into()),
        content: Some("TODO".into()),
        min_size: Some(10),
        max_size: Some(100),
        modified_after: Some(1),
        modified_before: Some(2),
        glob: Some("*.rs".into()),
        extensions: vec!["rs".into()],
        case_insensitive: true,
        exclude: vec!["target".into()],
        root: "C:\\a;C:\\b".into(),
        or_terms: vec!["x".into(), "y".into()],
        not_terms: vec!["z".into()],
        show_hidden: true,
    };
    let q = saved.to_query();
    assert_eq!(q.name_substr.as_deref(), Some("rep"));
    assert_eq!(q.regex.as_deref(), Some("a.*b"));
    assert_eq!(q.case_insensitive, true);
    assert_eq!(q.exclude, vec!["target".to_string()]);
    assert_eq!(q.or.len(), 2);
    assert_eq!(q.not.len(), 1);
    let mut back = SavedSearch::from_query("s", &q);
    // Roots and the hidden flag travel outside the query: the caller
    // stamps them on after converting (mirrors the GUI).
    back.root = saved.root.clone();
    back.show_hidden = true;
    assert_eq!(back, saved);
}

#[test]
#[test]
fn language_defaults_to_english_and_roundtrips() {
    // Old configs predate the key: they load as English.
    let old: AppConfig = serde_json::from_str(r#"{"roots":[]}"#).unwrap();
    assert_eq!(old.language, "en");
    assert_eq!(LangCode(old.language).as_lang(), "en");
    // Explicit codes survive a save/load round trip.
    let mut cfg = AppConfig::default();
    cfg.language = "ro".to_string();
    let back: AppConfig = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
    assert_eq!(back.language, "ro");
}

#[test]
fn ui_scale_defaults_and_sanitizes() {
    // Old configs without the key load unscaled.
    let old: AppConfig = serde_json::from_str(r#"{"roots":[]}"#).unwrap();
    assert_eq!(old.ui_scale, 1.0);
    assert_eq!(AppConfig::default().ui_scale, 1.0);
    // A picked scale survives a save/load round trip.
    let mut cfg = AppConfig::default();
    cfg.ui_scale = 1.5;
    let back: AppConfig = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
    assert_eq!(back.ui_scale, 1.5);
    // Out-of-range and non-finite values clamp to something sane.
    assert_eq!(sanitize_ui_scale(0.0), 0.5);
    assert_eq!(sanitize_ui_scale(9.0), 2.0);
    assert_eq!(sanitize_ui_scale(f32::NAN), 1.0);
}

/// Helper used only to document the code↔enum contract in one place.
struct LangCode(String);

impl LangCode {
    fn as_lang(&self) -> &'static str {
        match self.0.as_str() {
            "ro" => "ro",
            _ => "en",
        }
    }
}

#[test]
fn record_usage_remembers_roots_and_db() {
    let mut cfg = AppConfig::default();
    cfg.record_usage(&[PathBuf::from("/a")], None);
    assert_eq!(cfg.roots, vec![PathBuf::from("/a")]);
    assert!(cfg.db_path.is_none());
    // Empty roots must not wipe the remembered ones.
    cfg.record_usage(&[], Some(&PathBuf::from("/x.db")));
    assert_eq!(cfg.roots, vec![PathBuf::from("/a")]);
    assert_eq!(cfg.db_path, Some(PathBuf::from("/x.db")));
}

#[test]
fn legacy_string_recents_still_load() {
    // Config files written before snapshots stored bare strings.
    let text = r#"{"recents": ["report", "todo"]}"#;
    let cfg: AppConfig = serde_json::from_str(text).expect("deserialize");
    assert_eq!(
        cfg.recents,
        vec![
            RecentEntry::Legacy("report".to_string()),
            RecentEntry::Legacy("todo".to_string())
        ]
    );
    // And they serialize back as the same bare strings.
    let back = serde_json::to_string(&cfg).expect("serialize");
    assert!(back.contains(r#""recents":["report","todo"]"#));
}

#[test]
fn snapshot_recents_roundtrip_and_label() {
    let saved = SavedSearch {
        name: "x".into(),
        name_filter: Some("report".into()),
        content: Some("TODO".into()),
        ..SavedSearch::default()
    };
    let mut cfg = AppConfig::default();
    cfg.push_recent_search(saved.clone());
    // Re-pushing the same snapshot de-duplicates instead of doubling.
    cfg.push_recent_search(saved.clone());
    assert_eq!(cfg.recents, vec![RecentEntry::Full(saved.clone())]);
    assert_eq!(cfg.recents[0].label(), "report");
    let text = serde_json::to_string(&cfg).expect("serialize");
    let back: AppConfig = serde_json::from_str(&text).expect("deserialize");
    assert_eq!(back, cfg);
    // Empty (match-everything) snapshots are not remembered.
    cfg.push_recent_search(SavedSearch::default());
    assert_eq!(cfg.recents.len(), 1);
}

#[test]
fn snapshot_label_describes_filters_without_name() {
    let saved = SavedSearch {
        name: "x".into(),
        regex: Some("a.*b".into()),
        extensions: vec!["rs".into()],
        ..SavedSearch::default()
    };
    assert_eq!(RecentEntry::Full(saved).label(), "regex:a.*b ext:rs");
    let empty = SavedSearch {
        name: "x".into(),
        ..SavedSearch::default()
    };
    assert_eq!(RecentEntry::Full(empty).label(), "all files");
}

#[test]
fn csv_delim_defaults_to_comma_and_roundtrips() {
    // Old configs without the key load as comma.
    let cfg: AppConfig = serde_json::from_str(r#"{"roots": []}"#).expect("deserialize");
    assert_eq!(cfg.csv_delim, ',');
    assert_eq!(AppConfig::default().csv_delim, ',');
    // A picked delimiter survives disk round trips.
    let mut picked = AppConfig::default();
    picked.csv_delim = '\t';
    let text = serde_json::to_string(&picked).expect("serialize");
    let back: AppConfig = serde_json::from_str(&text).expect("deserialize");
    assert_eq!(back.csv_delim, '\t');
}

#[test]
fn default_config_path_ends_with_config_json() {
    let path = default_config_path();
    assert_eq!(
        path.file_name().and_then(|s| s.to_str()),
        Some("config.json")
    );
}

#[test]
fn snapshot_label_trims_name_and_covers_all_filters() {
    // A whitespace-only name falls back to the filter summary.
    let blank = SavedSearch {
        name: "x".into(),
        name_filter: Some("   ".into()),
        regex: Some("a".into()),
        ..SavedSearch::default()
    };
    assert_eq!(RecentEntry::Full(blank).label(), "regex:a");
    // Full summary order: regex fuzzy content glob ext.
    let full = SavedSearch {
        name: "x".into(),
        regex: Some("r".into()),
        fuzzy: Some("f".into()),
        content: Some("c".into()),
        glob: Some("*.rs".into()),
        extensions: vec!["rs".into(), "md".into()],
        ..SavedSearch::default()
    };
    assert_eq!(
        RecentEntry::Full(full).label(),
        "regex:r fuzzy:f content:c glob:*.rs ext:rs,md"
    );
    // Legacy entries pass through untouched.
    assert_eq!(RecentEntry::Legacy("old".to_string()).label(), "old");
    // Tooltips name the filters and the stored folders.
    let detailed = RecentEntry::Full(SavedSearch {
        name: "s".into(),
        or_terms: vec!["a".into()],
        root: "C:\\a".into(),
        ..SavedSearch::default()
    });
    let d = detailed.details();
    assert!(d.contains("any:a") && d.contains("C:\\a"), "was: {d}");
    assert_eq!(
        RecentEntry::Legacy("old".to_string()).details(),
        "old (name filter only)"
    );
    // from_query keeps the display name; the label prefers the name filter.
    let q = supersearch_query::Query {
        name_substr: Some("rep".into()),
        ..Default::default()
    };
    let saved = SavedSearch::from_query("My search", &q);
    assert_eq!(saved.name, "My search");
    assert_eq!(RecentEntry::Full(saved).label(), "rep");
    // OR/NOT terms join the summary when there is no name filter.
    let or_not = SavedSearch {
        name: "x".into(),
        or_terms: vec!["a".into()],
        not_terms: vec!["b".into()],
        ..SavedSearch::default()
    };
    assert_eq!(RecentEntry::Full(or_not).label(), "any:a not:b");
}

#[test]
fn file_group_upsert_validates_and_replaces() {
    let mut cfg = AppConfig::default();
    assert!(cfg
        .upsert_file_group(FileGroup {
            name: " ".into(),
            extensions: vec!["rs".into()],
        })
        .is_err());
    assert!(cfg
        .upsert_file_group(FileGroup {
            name: "X".into(),
            extensions: vec![],
        })
        .is_err());
    cfg.upsert_file_group(FileGroup {
        name: "Sheets".into(),
        extensions: vec![".xls".into(), " xlsx ".into()],
    })
    .unwrap();
    // Leading dots stripped, whitespace trimmed.
    assert_eq!(
        cfg.resolve_file_group("sheets"),
        Some(vec!["xls".to_string(), "xlsx".to_string()])
    );
    // Same name (any case) replaces instead of duplicating.
    cfg.upsert_file_group(FileGroup {
        name: "SHEETS".into(),
        extensions: vec!["ods".into()],
    })
    .unwrap();
    assert_eq!(cfg.file_groups.len(), 1);
    assert_eq!(
        cfg.resolve_file_group("sheets"),
        Some(vec!["ods".to_string()])
    );
}

#[test]
fn file_group_remove_reveals_shadowed_builtin() {
    let mut cfg = AppConfig::default();
    // Built-ins are not stored: nothing to delete.
    assert!(!cfg.remove_file_group("Images"));
    cfg.upsert_file_group(FileGroup {
        name: "Images".into(),
        extensions: vec!["qoi".into()],
    })
    .unwrap();
    assert_eq!(
        cfg.resolve_file_group("images"),
        Some(vec!["qoi".to_string()])
    );
    assert!(cfg.remove_file_group("IMAGES"));
    // The built-in shows again.
    let exts = cfg.resolve_file_group("images").unwrap();
    assert!(exts.contains(&"png".to_string()));
    assert!(!cfg.remove_file_group("Images"));
}
