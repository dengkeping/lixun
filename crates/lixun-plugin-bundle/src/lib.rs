//! Aggregates all opt-in lixun source plugins into a single crate that the
//! daemon links against. Each dependency's `inventory::submit!` entries
//! become part of the final binary's inventory pool automatically.
//!
//! Adding a new plugin: add it to `Cargo.toml` as an optional dep + new
//! feature, then `pub use <crate_name> as _;` below. The daemon does not
//! need to change.
//!
//! Removing a plugin: drop the feature from daemon's Cargo.toml. No code
//! change.

#[cfg(feature = "maildir")]
#[allow(unused_imports)]
use lixun_source_maildir as _;

#[cfg(feature = "thunderbird")]
#[allow(unused_imports)]
use lixun_source_thunderbird as _;

#[cfg(feature = "calculator")]
#[allow(unused_imports)]
use lixun_source_calculator as _;

#[cfg(feature = "shell")]
#[allow(unused_imports)]
use lixun_source_shell as _;
