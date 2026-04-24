//! Linker anchor for preview format plugins.
//!
//! Each tier-2 plugin task (G2.9–G2.15) adds three atomic edits
//! here and lands them together:
//!
//! 1. an optional path-dep on the plugin crate in `Cargo.toml`,
//! 2. a `<id> = ["dep:lixun-preview-<id>"]` entry under
//!    `[features]`,
//! 3. a `#[cfg(feature = "<id>")] use lixun_preview_<id> as _;`
//!    here so the plugin's `inventory::submit!` reaches the
//!    preview binary's link-time registry.
//!
//! In G2.8 the feature table is intentionally empty — Cargo cannot
//! declare `optional = true` dependencies on crates that do not
//! exist yet. This forces every plugin to land atomically as a
//! single commit covering crate + feature + linker use-stmt.
//!
//! The `use lixun_preview as _;` below keeps the trait crate
//! linked even when no plugins are enabled, so `select_plugin`
//! resolves cleanly against an empty registry.

use lixun_preview as _;

#[cfg(feature = "text")]
use lixun_preview_text as _;

#[cfg(feature = "image")]
use lixun_preview_image as _;

#[cfg(feature = "pdf")]
use lixun_preview_pdf as _;

#[cfg(feature = "code")]
use lixun_preview_code as _;

#[cfg(feature = "email")]
use lixun_preview_email as _;

#[cfg(feature = "office")]
use lixun_preview_office as _;

#[cfg(feature = "av")]
use lixun_preview_av as _;

#[cfg(test)]
mod tests {
    use lixun_core::{Action, Category, DocId, Hit};
    use std::path::PathBuf;

    fn text_hit() -> Hit {
        Hit {
            id: DocId("fs:/tmp/demo.txt".into()),
            category: Category::File,
            title: "demo.txt".into(),
            subtitle: String::new(),
            icon_name: None,
            kind_label: None,
            score: 0.0,
            action: Action::OpenFile {
                path: PathBuf::from("/tmp/demo.txt"),
            },
            extract_fail: false,
            sender: None,
            recipients: None,
            body: None,
            secondary_action: None,
        }
    }

    fn non_file_hit() -> Hit {
        Hit {
            id: DocId("app:firefox".into()),
            category: Category::App,
            title: "Firefox".into(),
            subtitle: String::new(),
            icon_name: None,
            kind_label: None,
            score: 0.0,
            action: Action::Launch {
                exec: "firefox".into(),
                terminal: false,
                desktop_id: None,
                desktop_file: None,
                working_dir: None,
            },
            extract_fail: false,
            sender: None,
            recipients: None,
            body: None,
            secondary_action: None,
        }
    }

    #[cfg(feature = "text")]
    #[test]
    fn text_feature_routes_txt_to_text_plugin() {
        let picked = lixun_preview::select_plugin(&text_hit())
            .expect("with `text` feature enabled a .txt hit must match some plugin");
        assert_eq!(
            picked.id(),
            "text",
            "expected the `text` plugin to win for a .txt hit"
        );
    }

    #[test]
    fn non_file_hits_do_not_match_any_registered_plugin() {
        assert!(
            lixun_preview::select_plugin(&non_file_hit()).is_none(),
            "app/launch hits must fall through select_plugin"
        );
    }
}
