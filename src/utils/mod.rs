//! Various utilities for docs.rs

pub(crate) use self::cargo_metadata::{CargoMetadata, Package as MetadataPackage};
pub(crate) use self::copy::copy_doc_dir;
pub use self::daemon::start_daemon;
pub use self::github_updater::GithubUpdater;
pub use self::gitlab_updater::GitlabUpdater;
pub(crate) use self::html::rewrite_lol;
pub use self::queue::{get_crate_priority, remove_crate_priority, set_crate_priority};
pub use self::queue_builder::queue_builder;
pub(crate) use self::rustc_version::parse_rustc_version;
pub use self::updater::Updater;
pub(crate) use self::updater::{RepositoryName, APP_USER_AGENT};

#[cfg(test)]
pub(crate) use self::cargo_metadata::{Dependency, Target};

mod cargo_metadata;
#[cfg(feature = "consistency_check")]
pub mod consistency;
mod copy;
mod daemon;
mod github_updater;
mod gitlab_updater;
mod html;
mod pubsubhubbub;
mod queue;
mod queue_builder;
mod rustc_version;
pub(crate) mod sized_buffer;
mod updater;
