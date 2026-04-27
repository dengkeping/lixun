use std::fs;
use std::io::Write;

use lixun_daemon::config::Config;

#[test]
fn test_default_config() {
    let cfg = Config::default();
    assert!(!cfg.roots.is_empty());
    assert!(cfg.max_file_size_mb > 0);
    assert!((cfg.ranking_apps - 1.3).abs() < 0.001);
    assert_eq!(cfg.keybindings.close, "Escape");
    assert_eq!(cfg.keybindings.global_toggle, "Super+space");
    assert!(cfg.exclude_regex.is_empty());
}

#[test]
fn test_expand_tilde() {
    let home = std::env::var("HOME").unwrap_or_default();
    let expanded = lixun_daemon::config::expand_tilde("~/Documents");
    assert!(expanded.to_string_lossy().contains("Documents"));
    if !home.is_empty() {
        assert!(expanded.starts_with(&home));
    }
}

#[test]
fn test_toml_config() {
    let tmp = tempfile::tempdir().unwrap();
    let config_dir = tmp.path().join("lixun");
    fs::create_dir_all(&config_dir).unwrap();
    let mut f = fs::File::create(config_dir.join("config.toml")).unwrap();
    writeln!(f, "max_file_size_mb = 100").unwrap();
    writeln!(f, "ranking = {{ apps = 2.0 }}").unwrap();
}

#[test]
fn user_exclude_merges_with_defaults() {
    let toml = r#"
        exclude = ["MyProject/tmp", ".vscode"]
    "#;
    let cfg = Config::from_toml_str(toml).unwrap();
    assert!(
        cfg.exclude.iter().any(|s| s == ".cache"),
        "default must survive"
    );
    assert!(
        cfg.exclude.iter().any(|s| s == "node_modules"),
        "default must survive"
    );
    assert!(
        cfg.exclude.iter().any(|s| s == "MyProject/tmp"),
        "user entry must be added"
    );
    assert!(
        cfg.exclude.iter().any(|s| s == ".vscode"),
        "user entry must be added"
    );
}

#[test]
fn exclude_regex_compiled_from_toml() {
    let toml = r#"
        exclude_regex = ['\.~lock\..*#$', '\.pyc$']
    "#;
    let cfg = Config::from_toml_str(toml).unwrap();
    assert_eq!(cfg.exclude_regex.len(), 2);
    assert!(
        cfg.exclude_regex
            .iter()
            .any(|r| r.is_match("/home/u/Docs/.~lock.Report.xlsx#"))
    );
    assert!(cfg.exclude_regex.iter().any(|r| r.is_match("/a/b/foo.pyc")));
}

#[test]
fn invalid_regex_patterns_dropped_not_fatal() {
    let toml = r#"
        exclude_regex = ['\.~lock\..*#$', '[unterminated', '\.tmp$']
    "#;
    let cfg = Config::from_toml_str(toml).unwrap();
    assert_eq!(
        cfg.exclude_regex.len(),
        2,
        "bad pattern dropped; good ones kept"
    );
}

#[test]
fn missing_exclude_sections_leave_defaults_intact() {
    let cfg = Config::from_toml_str("max_file_size_mb = 25").unwrap();
    let defaults = Config::default().exclude;
    assert_eq!(cfg.exclude, defaults);
    assert!(cfg.exclude_regex.is_empty());
    assert_eq!(cfg.max_file_size_mb, 25);
}

#[test]
fn empty_config_has_no_plugin_sections() {
    let cfg = Config::from_toml_str("").unwrap();
    assert!(cfg.plugin_sections.is_empty());
}

#[test]
fn unknown_top_level_keys_captured_as_plugin_sections() {
    let cfg = Config::from_toml_str(
        r#"
        max_file_size_mb = 25

        [thunderbird]
        enabled = true
        gloda_batch_size = 500

        [[maildir]]
        id = "personal"
        paths = ["~/Mail"]
    "#,
    )
    .unwrap();
    assert_eq!(cfg.plugin_sections.len(), 2);
    assert!(cfg.plugin_sections.contains_key("thunderbird"));
    assert!(cfg.plugin_sections.contains_key("maildir"));
}

#[test]
fn plugin_section_preserves_raw_toml_for_factory() {
    let cfg = Config::from_toml_str(
        r#"
        [thunderbird]
        gloda_batch_size = 1000
        profile = "~/.thunderbird/work"
    "#,
    )
    .unwrap();
    let raw = cfg.plugin_sections.get("thunderbird").unwrap();
    let table = raw.as_table().expect("thunderbird is a table");
    assert_eq!(
        table.get("gloda_batch_size").and_then(|v| v.as_integer()),
        Some(1000)
    );
    assert_eq!(
        table.get("profile").and_then(|v| v.as_str()),
        Some("~/.thunderbird/work")
    );
}

#[test]
fn plugin_section_maildir_preserved_as_array() {
    let cfg = Config::from_toml_str(
        r#"
        [[maildir]]
        id = "a"
        paths = ["/tmp/a"]

        [[maildir]]
        id = "b"
        paths = ["/tmp/b"]
    "#,
    )
    .unwrap();
    let raw = cfg.plugin_sections.get("maildir").unwrap();
    let arr = raw.as_array().expect("maildir is array-of-tables");
    assert_eq!(arr.len(), 2);
}

#[test]
fn known_keys_never_leak_into_plugin_sections() {
    let cfg = Config::from_toml_str(
        r#"
        roots = ["/tmp/custom"]
        exclude = [".foo"]
        max_file_size_mb = 100

        [ranking]
        apps = 2.0

        [keybindings]
        close = "Escape"

        [preview]
        enabled = false
    "#,
    )
    .unwrap();
    assert!(cfg.plugin_sections.is_empty());
    assert_eq!(cfg.max_file_size_mb, 100);
    assert!(!cfg.preview.enabled);
}

#[test]
fn preview_defaults_when_section_missing() {
    let cfg = Config::from_toml_str("").unwrap();
    assert!(cfg.preview.enabled);
    assert_eq!(cfg.preview.default_format, "auto");
    assert_eq!(cfg.preview.max_file_size_mb, 200);
    assert!(
        cfg.preview
            .cache_dir
            .to_string_lossy()
            .ends_with("lixun/preview"),
        "default cache_dir should be under the user cache dir: {:?}",
        cfg.preview.cache_dir
    );
}

#[test]
fn preview_full_override() {
    let cfg = Config::from_toml_str(
        r#"
        [preview]
        enabled = false
        default_format = "text"
        max_file_size_mb = 500
        cache_dir = "/var/tmp/lixun-preview"
    "#,
    )
    .unwrap();
    assert!(!cfg.preview.enabled);
    assert_eq!(cfg.preview.default_format, "text");
    assert_eq!(cfg.preview.max_file_size_mb, 500);
    assert_eq!(
        cfg.preview.cache_dir,
        std::path::PathBuf::from("/var/tmp/lixun-preview")
    );
}

#[test]
fn preview_partial_override_keeps_other_defaults() {
    let cfg = Config::from_toml_str(
        r#"
        [preview]
        max_file_size_mb = 1024
    "#,
    )
    .unwrap();
    assert!(cfg.preview.enabled, "enabled default (true) must survive");
    assert_eq!(
        cfg.preview.default_format, "auto",
        "default_format default must survive"
    );
    assert_eq!(cfg.preview.max_file_size_mb, 1024);
    let defaults = lixun_daemon::config::Config::default();
    assert_eq!(
        cfg.preview.cache_dir, defaults.preview.cache_dir,
        "cache_dir default must survive"
    );
}

#[test]
fn preview_cache_dir_tilde_expanded() {
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return;
    }
    let cfg = Config::from_toml_str(
        r#"
        [preview]
        cache_dir = "~/custom/preview"
    "#,
    )
    .unwrap();
    assert!(
        cfg.preview.cache_dir.starts_with(&home),
        "tilde must expand to $HOME: got {:?}",
        cfg.preview.cache_dir
    );
    assert!(
        cfg.preview.cache_dir.ends_with("custom/preview"),
        "path suffix preserved: got {:?}",
        cfg.preview.cache_dir
    );
}

#[test]
fn preview_preserves_nested_plugin_subtables() {
    let cfg = Config::from_toml_str(
        r##"
        [preview]
        enabled = true

        [preview.code]
        theme = "Solarized (light)"
        tab_size = 4

        [preview.image]
        background = "#222222"
    "##,
    )
    .unwrap();
    assert!(cfg.preview.enabled);
    assert!(
        !cfg.preview.plugin_sections.is_empty(),
        "preview.plugin_sections must capture nested subtables, got empty"
    );
    assert_eq!(cfg.preview.plugin_sections.len(), 2);

    let code = cfg
        .preview
        .plugin_sections
        .get("code")
        .expect("[preview.code] preserved")
        .as_table()
        .expect("[preview.code] is a table");
    assert_eq!(
        code.get("theme").and_then(|v| v.as_str()),
        Some("Solarized (light)")
    );
    assert_eq!(code.get("tab_size").and_then(|v| v.as_integer()), Some(4));

    let image = cfg
        .preview
        .plugin_sections
        .get("image")
        .expect("[preview.image] preserved")
        .as_table()
        .expect("[preview.image] is a table");
    assert_eq!(
        image.get("background").and_then(|v| v.as_str()),
        Some("#222222")
    );

    assert!(
        !cfg.plugin_sections.contains_key("preview"),
        "[preview] as a whole must not leak into top-level plugin_sections"
    );
    assert!(
        !cfg.plugin_sections.contains_key("code"),
        "nested [preview.code] must not leak into top-level plugin_sections"
    );
}

#[test]
fn preview_scalar_keys_do_not_contaminate_plugin_sections() {
    let cfg = Config::from_toml_str(
        r#"
        [preview]
        enabled = false
        default_format = "text"
        max_file_size_mb = 42
        cache_dir = "/tmp/x"

        [preview.code]
        theme = "foo"
    "#,
    )
    .unwrap();
    assert!(!cfg.preview.enabled);
    assert_eq!(cfg.preview.default_format, "text");
    assert_eq!(cfg.preview.max_file_size_mb, 42);
    assert_eq!(
        cfg.preview.plugin_sections.len(),
        1,
        "only the [preview.code] subtable should land in plugin_sections, \
         not the four scalar fields"
    );
    assert!(cfg.preview.plugin_sections.contains_key("code"));
    for scalar in ["enabled", "default_format", "max_file_size_mb", "cache_dir"] {
        assert!(
            !cfg.preview.plugin_sections.contains_key(scalar),
            "scalar key `{}` must not appear in preview.plugin_sections",
            scalar
        );
    }
}

#[test]
fn preview_does_not_leak_into_plugin_sections() {
    let cfg = Config::from_toml_str(
        r#"
        [preview]
        enabled = true
        max_file_size_mb = 250

        [thunderbird]
        enabled = true
    "#,
    )
    .unwrap();
    assert!(!cfg.plugin_sections.contains_key("preview"));
    assert!(cfg.plugin_sections.contains_key("thunderbird"));
    assert_eq!(cfg.preview.max_file_size_mb, 250);
}
