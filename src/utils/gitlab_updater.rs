use crate::error::Result;
use crate::{db::Pool, Config};
use chrono::{DateTime, Utc};
use log::{debug, info, trace, warn};
use once_cell::sync::Lazy;
use postgres::Client;
use regex::Regex;
use reqwest::{
    blocking::Client as HttpClient,
    header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT},
};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const APP_USER_AGENT: &str = concat!(
    env!("CARGO_PKG_NAME"),
    " ",
    include_str!(concat!(env!("OUT_DIR"), "/git_version"))
);

const GRAPHQL_UPDATE: &str = "query($ids: [ID!]!) {
    projects(ids: $ids) {
        nodes {
            id
            nameWithNamespace
            lastActivityAt
            description
            starCount
            forksCount
            openIssuesCount
        }
    }
}";

const GRAPHQL_SINGLE: &str = "query($fullPath: ID!) {
    project(fullPath: $fullPath) {
        id
        nameWithNamespace
        lastActivityAt
        description
        starCount
        forksCount
        openIssuesCount
    }
}";

/// How many repositories to update in a single chunk. Values over 100 are probably going to be
/// rejected by the GraphQL API.
const UPDATE_CHUNK_SIZE: usize = 100;

fn extract_host(url: &str) -> Option<&str> {
    url.split("//")
        .skip(1)
        .next()
        .and_then(|u| u.split('/').next())
}

pub struct GitlabUpdater {
    client: HttpClient,
    pool: Pool,
    config: Arc<Config>,
}

impl GitlabUpdater {
    /// Returns `Err` if the access token has invalid syntax (but *not* if it isn't authorized).
    /// Returns `Ok(None)` if there is no access token.
    pub fn new(config: Arc<Config>, pool: Pool) -> Result<Option<Self>> {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(APP_USER_AGENT));
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));

        if let Some(token) = &config.gitlab_accesstoken {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("token {}", token))?,
            );
        } else {
            return Ok(None);
        }

        let client = HttpClient::builder().default_headers(headers).build()?;

        Ok(Some(GitlabUpdater {
            client,
            pool,
            config,
        }))
    }

    pub fn backfill_repositories(&self) -> Result<()> {
        info!("started backfilling Gitlab repository stats");

        let mut conn = self.pool.get()?;
        let needs_backfilling = conn.query(
            "SELECT releases.id, crates.name, releases.version, releases.repository_url
             FROM releases
             INNER JOIN crates ON (crates.id = releases.crate_id)
             WHERE repository IS NULL AND repository_url LIKE '%gitlab.%/%';",
            &[],
        )?;

        let mut missing_urls = HashSet::new();
        for row in &needs_backfilling {
            let id: i32 = row.get("id");
            let name: String = row.get("name");
            let version: String = row.get("version");
            let url: String = row.get("repository_url");

            if missing_urls.contains(&url) {
                eprintln!("{} {} points to a known missing repo", name, version);
            } else if let Some(node_id) = self.load_repository(&mut conn, &url)? {
                conn.execute(
                    "UPDATE releases SET repository = $1 WHERE id = $2;",
                    &[&node_id, &id],
                )?;
                info!("backfilled Gitlab repository for {} {}", name, version);
            } else {
                eprintln!("{} {} does not point to a Gitlab repository", name, version);
                missing_urls.insert(url);
            }
        }

        Ok(())
    }

    pub(crate) fn load_repository(&self, conn: &mut Client, url: &str) -> Result<Option<i32>> {
        let name = match RepositoryName::from_url(url) {
            Some(name) => name,
            None => return Ok(None),
        };

        if let Some(host) = extract_host(url) {
            let project_path = format!("{}/{}", name.owner, name.repo);
            // Avoid querying the Gitlab API for repositories we already loaded.
            if let Some(row) = conn.query_opt(
                "SELECT id FROM repositories WHERE name = $1 AND host = $2 LIMIT 1;",
                &[&project_path, &host],
            )? {
                return Ok(Some(row.get("id")));
            }

            // Fetch the latest information from the Gitlab API.
            let response: GraphResponse<GraphProjectNode> = self.graphql(
                host,
                GRAPHQL_SINGLE,
                serde_json::json!({
                    "fullPath": &project_path,
                }),
            )?;
            if let Some(repo) = response.data.and_then(|d| d.project) {
                Ok(Some(self.store_repository(host, conn, &repo)?))
            } else if let Some(error) = response.errors.get(0) {
                failure::bail!("error loading repository: {}", error.message)
            } else {
                self.delete_repository(conn, &project_path, url)?;
                Ok(None)
            }
        } else {
            failure::bail!("failed to extract host from `{}`", url)
        }
    }

    /// Updates gitlab fields in crates table
    pub fn update_all_crates(&self) -> Result<()> {
        info!("started updating Gitlab repository stats");

        let mut conn = self.pool.get()?;
        let needs_update = conn
            .query(
                "SELECT repositories.host_id, releases.repository_url
                 FROM repositories
                 INNER JOIN releases ON (releases.repository = repositories.id)
                 WHERE host != 'github' AND updated_at < NOW() - INTERVAL '1 day';",
                &[],
            )?
            .into_iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect::<Vec<(String, String)>>();

        if needs_update.is_empty() {
            info!("no Gitlab repository stats needed to be updated");
            return Ok(());
        }

        let mut url_map: HashMap<String, Vec<&str>> = HashMap::new();

        for (chunk, url) in &needs_update {
            if let Some(url) = extract_host(url) {
                url_map.entry(url.to_owned()).or_default().push(chunk);
            } else {
                warn!("Couldn't extract host from `{}`", url);
            }
        }

        for (url, chunks) in &url_map {
            for chunk in chunks.chunks(UPDATE_CHUNK_SIZE) {
                if let Err(err) = self.update_repositories(url, &mut conn, &chunk) {
                    if err.downcast_ref::<RateLimitReached>().is_some() {
                        warn!("rate limit reached, blocked the Gitlab repository stats updater");
                        return Ok(());
                    }
                    return Err(err);
                }
            }
        }

        info!("finished updating Gitlab repository stats");
        Ok(())
    }

    fn update_repositories(&self, url: &str, conn: &mut Client, node_ids: &[&str]) -> Result<()> {
        let response: GraphResponse<GraphProjects<Option<GraphProject>>> = self.graphql(
            url,
            GRAPHQL_UPDATE,
            serde_json::json!({
                "ids": node_ids,
            }),
        )?;

        // The error is returned *before* we reach the rate limit, to ensure we always have an
        // amount of API calls we can make at any time.
        if let Some(data) = response.data {
            // trace!(
            //     "Gitlab GraphQL rate limit remaining: {}",
            //     data.rate_limit.remaining
            // );
            // if data.rate_limit.remaining < self.config.gitlab_updater_min_rate_limit {
            //     return Err(RateLimitReached.into());
            // }

            // When a node is missing (for example if the repository was deleted or made private) the
            // GraphQL API will return *both* a `null` instead of the data in the nodes list and a
            // `NOT_FOUND` error in the errors list.
            for node in &data.projects.nodes {
                if let Some(node) = node {
                    self.store_repository(url, conn, &node)?;
                }
            }
            for error in &response.errors {
                failure::bail!("error updating repositories: {}", error.message);
            }

            Ok(())
        } else {
            failure::bail!("no data")
        }
    }

    fn graphql<T: serde::de::DeserializeOwned + std::fmt::Debug>(
        &self,
        host: &str,
        query: &str,
        variables: impl serde::Serialize,
    ) -> Result<GraphResponse<T>> {
        eprintln!("doing stuff on {:?}", host);
        eprintln!("+++> {:?}", query);
        Ok(self
            .client
            .post(&format!("https://{}/api/graphql", host))
            .json(&serde_json::json!({
                "query": query,
                "variables": variables,
            }))
            .send()?
            .error_for_status()?
            .json()?)
        // let tmp = self
        //     .client
        //     .post(&format!("https://{}/api/graphql", host))
        //     .json(&serde_json::json!({
        //         "query": query,
        //         "variables": variables,
        //     }))
        //     .send()?
        //     .error_for_status()?;
        // eprintln!("---> {:?}", tmp.headers());
        // let s: String = tmp.text()?;
        // eprintln!("---> {:?}", s);
        // panic!("yolo");
    }

    fn store_repository(&self, host: &str, conn: &mut Client, repo: &GraphProject) -> Result<i32> {
        trace!(
            "storing Gitlab repository stats for {}",
            repo.name_with_namespace
        );
        let rows = conn.query(
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
                &repo.id,
                &repo.name_with_namespace,
                &repo.description,
                &repo.last_activity_at,
                &(repo.star_count as i32),
                &(repo.forks_count as i32),
                &(repo.open_issues_count as i32),
            ],
        )?;
        Ok(rows[0].get(0))
    }

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
struct RepositoryName<'a> {
    owner: &'a str,
    repo: &'a str,
}

const HOSTS: &[&str] = &["gitlab.com", "gitlab.freedesktop.org"];

impl<'a> RepositoryName<'a> {
    fn from_url(url: &'a str) -> Option<Self> {
        static RE: Lazy<Regex> = Lazy::new(|| {
            Regex::new(r"https?://(?P<host>[\w\.]*gitlab\.[\w\.?]+)/(?P<owner>[\w\._-]+)/(?P<repo>[\w\._-]+)")
                .unwrap()
        });

        match RE.captures(url) {
            Some(cap) => {
                let host = cap.name("host").expect("missing group 'host'").as_str();
                if !HOSTS.iter().any(|s| *s == host) {
                    return None;
                }
                let owner = cap.name("owner").expect("missing group 'owner'").as_str();
                let repo = cap.name("repo").expect("missing group 'repo'").as_str();
                Some(Self {
                    owner,
                    repo: repo.strip_suffix(".git").unwrap_or(repo),
                })
            }
            None => None,
        }
    }
}

#[derive(Debug, failure::Fail)]
#[fail(display = "rate limit reached")]
struct RateLimitReached;

#[derive(Debug, Deserialize)]
struct GraphProjects<T> {
    projects: GraphNodes<T>,
}

#[derive(Debug, Deserialize)]
struct GraphResponse<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphError>,
}

#[derive(Debug, Deserialize)]
struct GraphError {
    message: String,
    locations: Vec<GraphErrorLocation>,
}

#[derive(Debug, Deserialize)]
struct GraphErrorLocation {
    line: u32,
    column: u32,
}

#[derive(Debug, Deserialize)]
struct GraphRateLimit {
    remaining: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphNodes<T> {
    nodes: Vec<T>,
}

#[derive(Debug, Deserialize)]
struct GraphProjectNode {
    project: Option<GraphProject>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphProject {
    id: String,
    name_with_namespace: String,
    last_activity_at: Option<DateTime<Utc>>,
    description: Option<String>,
    star_count: i64,
    forks_count: i64,
    open_issues_count: i64,
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_repository_name() {
        macro_rules! assert_name {
            ($url:expr => ($owner:expr, $repo: expr)) => {
                assert_eq!(
                    RepositoryName::from_url($url),
                    Some(RepositoryName {
                        owner: $owner,
                        repo: $repo
                    })
                );
            };
        }

        assert_name!("https://gitlab.com/onur/cratesfyi" => ("onur", "cratesfyi"));
        assert_name!("http://gitlab.com/onur/cratesfyi" => ("onur", "cratesfyi"));
        assert_name!("https://www.gitlab.com/onur/cratesfyi" => ("onur", "cratesfyi"));
        assert_name!("http://www.gitlab.com/onur/cratesfyi" => ("onur", "cratesfyi"));
        assert_name!("https://gitlab.com/onur/cratesfyi.git" => ("onur", "cratesfyi"));
        assert_name!("https://gitlab.com/docopt/docopt.rs" => ("docopt", "docopt.rs"));
        assert_name!("https://gitlab.com/onur23cmD_M_R_L_/crates_fy-i" => (
            "onur23cmD_M_R_L_", "crates_fy-i"
        ));
        assert_name!("https://freedesktop.gitlab.org/test/test" => (
            "test", "test"
        ));
        assert_name!("https://gitlab.freedesktop.org/test/test" => (
            "test", "test"
        ));
    }
}
