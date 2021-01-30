pub use self::github_updater::GithubUpdater;
pub use self::gitlab_updater::GitlabUpdater;
#[cfg(test)]
pub(crate) use self::updater::RepositoryName;
pub use self::updater::{RepositoryStatsUpdater, Updater, get_icon_name};

pub const APP_USER_AGENT: &str = concat!(
    env!("CARGO_PKG_NAME"),
    " ",
    include_str!(concat!(env!("OUT_DIR"), "/git_version"))
);

mod github_updater;
mod gitlab_updater;
mod updater;
