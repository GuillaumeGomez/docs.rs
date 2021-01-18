use crate::error::Result;
use crate::utils::daemon::cron;
use crate::utils::{GithubUpdater, GitlabUpdater, MetadataPackage};
use crate::{db::Pool, Config, Context};
use log::{debug, trace, warn};
use postgres::Client;
use std::sync::Arc;
use std::time::Duration;

pub const APP_USER_AGENT: &str = concat!(
    env!("CARGO_PKG_NAME"),
    " ",
    include_str!(concat!(env!("OUT_DIR"), "/git_version"))
);

pub trait Updater {
    fn new(config: Arc<Config>, pool: Pool) -> Result<Option<Self>>
    where
        Self: Sized;
    fn backfill_repositories(&self) -> Result<()>;
    fn load_repository(&self, conn: &mut Client, url: &str) -> Result<Option<i32>>;
    fn update_all_crates(&self) -> Result<()>;
    fn repository_name(url: &str) -> Option<RepositoryName>;
    fn name() -> &'static str;
    fn delete_repository(&self, conn: &mut Client, host_id: &str, host: &str) -> Result<()> {
        trace!(
            "removing {} repository stats for host ID `{}` and host `{}`",
            Self::name(),
            host_id,
            host
        );
        conn.execute(
            "DELETE FROM repositories WHERE host_id = $1 AND host = $2;",
            &[&host_id, &host],
        )?;
        Ok(())
    }
}

pub struct RepositoryStatsUpdater;

impl RepositoryStatsUpdater {
    pub(crate) fn load_repository(
        conn: &mut Client,
        metadata: &MetadataPackage,
        config: Arc<Config>,
        db: Pool,
    ) -> Result<Option<i32>> {
        macro_rules! return_if_ok_some {
            ($typ:ty) => {
                let data =
                    Self::load_repository_inner::<$typ>(conn, metadata, config.clone(), db.clone());
                if matches!(data, Ok(Some(_))) {
                    return data;
                }
            };
        }

        // The `GitlabUpdater is a bit more permissive so better to put it at the end.
        return_if_ok_some!(GithubUpdater);
        return_if_ok_some!(GitlabUpdater);
        Ok(None)
    }

    fn load_repository_inner<T: Updater>(
        conn: &mut Client,
        metadata: &MetadataPackage,
        config: Arc<Config>,
        db: Pool,
    ) -> Result<Option<i32>> {
        let updater = match T::new(config, db)? {
            Some(updater) => updater,
            None => {
                return Ok(None);
            }
        };
        let repo = match &metadata.repository {
            Some(url) => url,
            None => {
                debug!(
                    "did not collect {} stats as no repository URL was present",
                    T::name()
                );
                return Ok(None);
            }
        };
        match updater.load_repository(conn, repo) {
            Ok(repo) => Ok(repo),
            Err(err) => {
                warn!("failed to collect {} stats: {}", T::name(), err);
                Ok(None)
            }
        }
    }

    pub fn start_crons(config: Arc<Config>, context: &dyn Context) -> Result<()> {
        macro_rules! start_cron {
            ($typ:ty) => {
                if let Some(updater) = <$typ>::new(config.clone(), context.pool()?)? {
                    cron(
                        concat!(stringify!($typ), " stats updater"),
                        Duration::from_secs(60 * 60),
                        move || {
                            updater.update_all_crates()?;
                            Ok(())
                        },
                    )?;
                } else {
                    log::warn!(
                        "{} stats updater not started as no token was provided",
                        <$typ>::name()
                    );
                }
            };
        }

        start_cron!(GithubUpdater);
        start_cron!(GitlabUpdater);
        Ok(())
    }

    pub fn update_all_crates(ctx: &dyn Context) -> Result<()> {
        fn inner<T: Updater>(ctx: &dyn Context) -> Result<()> {
            match T::new(ctx.config()?, ctx.pool()?)? {
                Some(up) => up.update_all_crates()?,
                None => warn!("missing {} token", T::name()),
            }
            Ok(())
        }

        inner::<GithubUpdater>(ctx)?;
        inner::<GitlabUpdater>(ctx)?;
        Ok(())
    }

    pub fn backfill_repositories(ctx: &dyn Context) -> Result<()> {
        fn inner<T: Updater>(ctx: &dyn Context) -> Result<()> {
            match T::new(ctx.config()?, ctx.pool()?)? {
                Some(up) => up.backfill_repositories()?,
                None => warn!("missing {} token", T::name()),
            }
            Ok(())
        }

        inner::<GithubUpdater>(ctx)?;
        inner::<GitlabUpdater>(ctx)?;
        Ok(())
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct RepositoryName<'a> {
    pub owner: &'a str,
    pub repo: &'a str,
}
