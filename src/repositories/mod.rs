pub use self::github::GitHub;
pub use self::gitlab::GitLab;
#[cfg(test)]
pub(crate) use self::updater::RepositoryName;
pub use self::updater::{get_icon_name, RepositoryStatsUpdater, Updater};

pub const APP_USER_AGENT: &str = concat!(
    env!("CARGO_PKG_NAME"),
    " ",
    include_str!(concat!(env!("OUT_DIR"), "/git_version"))
);

mod github;
mod gitlab;
mod updater;
