use crate::error::Result;
use crate::repositories::{GithubUpdater, GitlabUpdater};
use crate::utils::{daemon::cron, MetadataPackage};
use crate::{db::Pool, Config, Context};
use chrono::{DateTime, Utc};
use log::{debug, info, trace, warn};
use once_cell::sync::Lazy;
use postgres::Client;
use regex::Regex;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

pub trait Updater {
    fn new(config: Arc<Config>, pool: Pool) -> Result<Option<Self>>
    where
        Self: Sized;
    fn load_repository(&self, conn: &mut Client, url: &str) -> Result<Option<i32>>;
    fn update_all_crates(&self) -> Result<()>;
    fn name() -> &'static str;
    fn hosts() -> &'static [&'static str];
    fn pool(&self) -> &Pool;

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

    #[allow(clippy::too_many_arguments)]
    fn store_repository(
        &self,
        conn: &mut Client,
        host: &str,
        repo_id: &str,
        name_with_owner: &str,
        description: &Option<String>,
        last_activity_at: &Option<DateTime<Utc>>,
        star_count: i64,
        fork_count: i64,
        open_issues_count: i64,
    ) -> Result<i32> {
        trace!(
            "storing {} repository stats for {}",
            Self::name(),
            name_with_owner,
        );
        let data = conn.query_one(
            "INSERT INTO repositories (
                 host, host_id, name, description, last_commit, stars, forks, issues, updated_at
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW())
             ON CONFLICT (host, host_id) DO
             UPDATE SET
                 name = $3,
                 description = $4,
                 last_commit = $5,
                 stars = $6,
                 forks = $7,
                 issues = $8,
                 updated_at = NOW()
             RETURNING id;",
            &[
                &host,
                &repo_id,
                &name_with_owner,
                &description,
                &last_activity_at,
                &(star_count as i32),
                &(fork_count as i32),
                &(open_issues_count as i32),
            ],
        )?;
        Ok(data.get(0))
    }

    fn backfill_repositories(&self) -> Result<()> {
        info!("started backfilling {} repository stats", Self::name());

        let mut conn = self.pool().get()?;
        for host in Self::hosts() {
            let needs_backfilling = conn.query(
                "SELECT releases.id, crates.name, releases.version, releases.repository_url
                 FROM releases
                 INNER JOIN crates ON (crates.id = releases.crate_id)
                 WHERE repository_id IS NULL AND repository_url LIKE $1;",
                &[&format!("%{}%", host)],
            )?;

            let mut missing_urls = HashSet::new();
            for row in &needs_backfilling {
                let id: i32 = row.get("id");
                let name: String = row.get("name");
                let version: String = row.get("version");
                let url: String = row.get("repository_url");

                if missing_urls.contains(&url) {
                    debug!("{} {} points to a known missing repo", name, version);
                } else if let Some(node_id) = self.load_repository(&mut conn, &url)? {
                    conn.execute(
                        "UPDATE releases SET repository_id = $1 WHERE id = $2;",
                        &[&node_id, &id],
                    )?;
                    info!(
                        "backfilled {} repository for {} {}",
                        Self::name(),
                        name,
                        version
                    );
                } else {
                    debug!(
                        "{} {} does not point to a {} repository",
                        Self::name(),
                        name,
                        version
                    );
                    missing_urls.insert(url);
                }
            }
        }

        Ok(())
    }

    fn repository_name(url: &str) -> Option<RepositoryName> {
        static RE: Lazy<Regex> = Lazy::new(|| {
            Regex::new(r"https?://(?P<host>.+)/(?P<owner>[\w\._-]+)/(?P<repo>[\w\._-]+)").unwrap()
        });

        let cap = RE.captures(url)?;
        let host = cap.name("host").expect("missing group 'host'").as_str();
        if !Self::hosts().iter().any(|s| *s == host) {
            return None;
        }
        let owner = cap.name("owner").expect("missing group 'owner'").as_str();
        let repo = cap.name("repo").expect("missing group 'repo'").as_str();
        Some(RepositoryName {
            owner,
            repo: repo.strip_suffix(".git").unwrap_or(repo),
            host,
        })
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
    pub host: &'a str,
}

pub fn get_icon_name(host: &str) -> &'static str {
    if GithubUpdater::hosts().iter().any(|&h| h == host) {
        "github"
    } else {
        "gitlab"
    }
}
