use crate::error::Result;
use crate::{db::Pool, Config};
use chrono::{DateTime, Utc};
use log::{info, trace, warn};
use postgres::Client;
use reqwest::{
    blocking::Client as HttpClient,
    header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT},
};
use serde::Deserialize;
use std::sync::Arc;

use crate::repositories::{Updater, APP_USER_AGENT};

const GRAPHQL_UPDATE: &str = "query($ids: [ID!]!) {
    nodes(ids: $ids) {
        ... on Repository {
            id
            nameWithOwner
            pushedAt
            description
            stargazerCount
            forkCount
            issues(states: [OPEN]) { totalCount }
        }
    }
    rateLimit {
        remaining
    }
}";

const GRAPHQL_SINGLE: &str = "query($owner: String!, $repo: String!) {
    repository(owner: $owner, name: $repo) {
        id
        nameWithOwner
        pushedAt
        description
        stargazerCount
        forkCount
        issues(states: [OPEN]) { totalCount }
    }
}";

/// How many repositories to update in a single chunk. Values over 100 are probably going to be
/// rejected by the GraphQL API.
const UPDATE_CHUNK_SIZE: usize = 100;

pub struct GithubUpdater {
    client: HttpClient,
    pool: Pool,
    config: Arc<Config>,
}

impl Updater for GithubUpdater {
    /// Returns `Err` if the access token has invalid syntax (but *not* if it isn't authorized).
    /// Returns `Ok(None)` if there is no access token.
    fn new(config: Arc<Config>, pool: Pool) -> Result<Option<Self>> {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(APP_USER_AGENT));
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));

        if let Some(token) = &config.github_accesstoken {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("token {}", token))?,
            );
        } else {
            warn!("did not collect GitHub stats as no token was provided");
            return Ok(None);
        }

        let client = HttpClient::builder().default_headers(headers).build()?;

        Ok(Some(GithubUpdater {
            client,
            pool,
            config,
        }))
    }

    fn load_repository(&self, conn: &mut Client, url: &str) -> Result<Option<i32>> {
        let name = match Self::repository_name(url) {
            Some(name) => name,
            None => return Ok(None),
        };

        // Avoid querying the GitHub API for repositories we already loaded.
        if let Some(row) = conn.query_opt(
            "SELECT id FROM repositories WHERE name = $1 AND host = $2 LIMIT 1;",
            &[&format!("{}/{}", name.owner, name.repo), &name.host],
        )? {
            return Ok(Some(row.get("id")));
        }

        // Fetch the latest information from the GitHub API.
        let response: GraphResponse<GraphRepositoryNode> = self.graphql(
            GRAPHQL_SINGLE,
            serde_json::json!({
                "owner": name.owner,
                "repo": name.repo,
            }),
        )?;
        if let Some(repo) = response.data.repository {
            Ok(Some(self.store_repository(
                conn,
                Self::hosts()[0],
                &repo.id,
                &repo.name_with_owner,
                &repo.description,
                &repo.pushed_at,
                repo.stargazer_count,
                repo.fork_count,
                repo.issues.total_count,
            )?))
        } else if let Some(error) = response.errors.get(0) {
            use GraphErrorPath::*;
            match (error.error_type.as_str(), error.path.as_slice()) {
                ("NOT_FOUND", [Segment(repository)]) if repository == "repository" => Ok(None),
                _ => failure::bail!("error loading repository: {}", error.message),
            }
        } else {
            panic!("missing repository but there were no errors!");
        }
    }

    /// Updates github fields in crates table
    fn update_all_crates(&self) -> Result<()> {
        info!("started updating GitHub repository stats");

        let mut updated = 0;
        let mut conn = self.pool.get()?;
        for host in Self::hosts() {
            let needs_update = conn
                .query(
                    "SELECT host_id
                     FROM repositories
                     WHERE host = $1 AND updated_at < NOW() - INTERVAL '1 day';",
                    &[&host],
                )?
                .into_iter()
                .map(|row| row.get(0))
                .collect::<Vec<String>>();

            for chunk in needs_update.chunks(UPDATE_CHUNK_SIZE) {
                if let Err(err) = self.update_repositories(&mut conn, &chunk) {
                    if err.downcast_ref::<RateLimitReached>().is_some() {
                        warn!("rate limit reached, blocked the GitHub repository stats updater");
                        return Ok(());
                    }
                    return Err(err);
                }
            }

            updated += needs_update.len();
        }

        if updated == 0 {
            info!("no GitHub repository stats needed to be updated");
        } else {
            info!("finished updating GitHub repository stats");
        }
        Ok(())
    }

    fn name() -> &'static str {
        "Github"
    }

    fn hosts() -> &'static [&'static str] {
        &["github.com"]
    }

    fn pool(&self) -> &Pool {
        &self.pool
    }
}

impl GithubUpdater {
    fn update_repositories(&self, conn: &mut Client, node_ids: &[String]) -> Result<()> {
        let response: GraphResponse<GraphNodes<Option<GraphRepository>>> = self.graphql(
            GRAPHQL_UPDATE,
            serde_json::json!({
                "ids": node_ids,
            }),
        )?;

        // The error is returned *before* we reach the rate limit, to ensure we always have an
        // amount of API calls we can make at any time.
        trace!(
            "GitHub GraphQL rate limit remaining: {}",
            response.data.rate_limit.remaining
        );
        if response.data.rate_limit.remaining < self.config.github_updater_min_rate_limit {
            return Err(RateLimitReached.into());
        }

        let host = Self::hosts()[0];

        // When a node is missing (for example if the repository was deleted or made private) the
        // GraphQL API will return *both* a `null` instead of the data in the nodes list and a
        // `NOT_FOUND` error in the errors list.
        for node in &response.data.nodes {
            if let Some(node) = node {
                self.store_repository(
                    conn,
                    host,
                    &node.id,
                    &node.name_with_owner,
                    &node.description,
                    &node.pushed_at,
                    node.stargazer_count,
                    node.fork_count,
                    node.issues.total_count,
                )?;
            }
        }
        for error in &response.errors {
            use GraphErrorPath::*;
            match (error.error_type.as_str(), error.path.as_slice()) {
                ("NOT_FOUND", [Segment(nodes), Index(idx)]) if nodes == "nodes" => {
                    self.delete_repository(conn, &node_ids[*idx as usize], host)?;
                }
                _ => failure::bail!("error updating repositories: {}", error.message),
            }
        }

        Ok(())
    }

    fn graphql<T: serde::de::DeserializeOwned>(
        &self,
        query: &str,
        variables: impl serde::Serialize,
    ) -> Result<GraphResponse<T>> {
        Ok(self
            .client
            .post("https://api.github.com/graphql")
            .json(&serde_json::json!({
                "query": query,
                "variables": variables,
            }))
            .send()?
            .error_for_status()?
            .json()?)
    }
}

#[derive(Debug, failure::Fail)]
#[fail(display = "rate limit reached")]
struct RateLimitReached;

#[derive(Debug, Deserialize)]
struct GraphResponse<T> {
    data: T,
    #[serde(default)]
    errors: Vec<GraphError>,
}

#[derive(Debug, Deserialize)]
struct GraphError {
    #[serde(rename = "type")]
    error_type: String,
    path: Vec<GraphErrorPath>,
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum GraphErrorPath {
    Segment(String),
    Index(i64),
}

#[derive(Debug, Deserialize)]
struct GraphRateLimit {
    remaining: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphNodes<T> {
    nodes: Vec<T>,
    rate_limit: GraphRateLimit,
}

#[derive(Debug, Deserialize)]
struct GraphRepositoryNode {
    repository: Option<GraphRepository>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphRepository {
    id: String,
    name_with_owner: String,
    pushed_at: Option<DateTime<Utc>>,
    description: Option<String>,
    stargazer_count: i64,
    fork_count: i64,
    issues: GraphIssues,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphIssues {
    total_count: i64,
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::repositories::RepositoryName;

    #[test]
    fn test_repository_name() {
        macro_rules! assert_name {
            ($url:expr => ($owner:expr, $repo:expr, $host:expr)) => {
                assert_eq!(
                    GithubUpdater::repository_name($url),
                    Some(RepositoryName {
                        owner: $owner,
                        repo: $repo,
                        host: $host,
                    })
                );
            };
            ($url:expr => None) => {
                assert_eq!(GithubUpdater::repository_name($url), None);
            };
        }

        assert_name!("https://github.com/onur/cratesfyi" => ("onur", "cratesfyi", "github.com"));
        assert_name!("http://github.com/onur/cratesfyi" => ("onur", "cratesfyi", "github.com"));
        assert_name!("https://github.com/onur/cratesfyi.git" => ("onur", "cratesfyi", "github.com"));
        assert_name!("https://github.com/docopt/docopt.rs" => ("docopt", "docopt.rs", "github.com"));
        assert_name!("https://github.com/onur23cmD_M_R_L_/crates_fy-i" => (
            "onur23cmD_M_R_L_", "crates_fy-i", "github.com"
        ));
        assert_name!("https://www.github.com/onur/cratesfyi" => None);
        assert_name!("http://www.github.com/onur/cratesfyi" => None);
        assert_name!("http://www.gitlab.com/onur/cratesfyi" => None);
    }
}
