use crate::error::Result;
use crate::{db::Pool, Config};
use log::trace;
use postgres::Client;
use std::sync::Arc;

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
    fn delete_repository(&self, conn: &mut Client, host_id: &str, host: &str) -> Result<()> {
        trace!(
            "removing Gitlab repository stats for host ID `{}` and host `{}`",
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

#[derive(Debug, Eq, PartialEq)]
pub struct RepositoryName<'a> {
    pub owner: &'a str,
    pub repo: &'a str,
}
