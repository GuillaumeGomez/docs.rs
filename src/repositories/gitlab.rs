use crate::error::Result;
use crate::{db::Pool, Config};
use chrono::{DateTime, Utc};
use log::{info, warn};
use postgres::Client;
use reqwest::{
    blocking::Client as HttpClient,
    header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT},
};
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::Arc;

use crate::repositories::{Updater, APP_USER_AGENT};

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

/// How many repositories to update in a single chunk.
const UPDATE_CHUNK_SIZE: usize = 5;

pub struct GitLab {
    client: HttpClient,
    pool: Pool,
}

impl Updater for GitLab {
    /// Returns `Err` if the access token has invalid syntax (but *not* if it isn't authorized).
    /// Returns `Ok(None)` if there is no access token.
    fn new(config: Arc<Config>, pool: Pool) -> Result<Option<Self>> {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(APP_USER_AGENT));
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));

        if let Some(token) = &config.gitlab_accesstoken {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {}", token))?,
            );
        } else {
            warn!("will try to retrieve Gitlab stats without token since none was provided");
        }

        let client = HttpClient::builder().default_headers(headers).build()?;

        Ok(Some(GitLab { client, pool }))
    }

    fn load_repository(&self, conn: &mut Client, url: &str) -> Result<Option<i32>> {
        let name = match Self::repository_name(url) {
            Some(name) => name,
            None => return Ok(None),
        };

        let project_path = format!("{}/{}", name.owner, name.repo);
        // Avoid querying the Gitlab API for repositories we already loaded.
        if let Some(row) = conn.query_opt(
            "SELECT id FROM repositories WHERE name = $1 AND host = $2 LIMIT 1;",
            &[&project_path, &name.host],
        )? {
            return Ok(Some(row.get("id")));
        }

        // Fetch the latest information from the Gitlab API.
        let response: GraphResponse<GraphProjectNode> = self.graphql(
            name.host,
            GRAPHQL_SINGLE,
            serde_json::json!({
                "fullPath": &project_path,
            }),
        )?;
        if let Some(repo) = response.data.and_then(|d| d.project) {
            Ok(Some(self.store_repository(
                conn,
                name.host,
                &repo.id,
                &repo.name_with_namespace,
                &repo.description,
                &repo.last_activity_at,
                repo.star_count,
                repo.forks_count,
                repo.open_issues_count.unwrap_or(0),
            )?))
        } else if let Some(error) = response.errors.get(0) {
            failure::bail!("error loading repository: {}", error.message)
        } else {
            // When an ID isn't found, gitlab doesn't return an error, it returns a `project` with
            // `null` as value.
            Ok(None)
        }
    }

    /// Updates gitlab fields in crates table
    fn update_all_crates(&self) -> Result<()> {
        info!("started updating Gitlab repository stats");

        let mut updated = 0;
        let mut conn = self.pool.get()?;
        for host in Self::hosts() {
            let needs_update = conn
                .query(
                    "SELECT host_id
                     FROM repositories
                     WHERE host = $1 AND updated_at < NOW() - INTERVAL '1 day';",
                    &[host],
                )?
                .into_iter()
                .map(|row| row.get(0))
                .collect::<Vec<String>>();

            for chunk in needs_update.chunks(UPDATE_CHUNK_SIZE) {
                if let Err(err) = self.update_repositories(host, &mut conn, &chunk) {
                    if err.downcast_ref::<RateLimitReached>().is_some() {
                        warn!("rate limit reached, blocked the Gitlab repository stats updater");
                        return Ok(());
                    }
                    return Err(err);
                }
            }

            updated += needs_update.len();
        }

        if updated == 0 {
            info!("no Gitlab repository stats needed to be updated");
        } else {
            info!("finished updating Gitlab repository stats");
        }
        Ok(())
    }

    fn name() -> &'static str {
        "Gitlab"
    }

    fn hosts() -> &'static [&'static str] {
        &["gitlab.com", "gitlab.freedesktop.org"]
    }

    fn pool(&self) -> &Pool {
        &self.pool
    }
}

impl GitLab {
    fn update_repositories(
        &self,
        host: &str,
        conn: &mut Client,
        node_ids: &[String],
    ) -> Result<()> {
        let response: GraphResponse<GraphProjects<Option<GraphProject>>> = self.graphql(
            host,
            GRAPHQL_UPDATE,
            serde_json::json!({
                "ids": node_ids,
            }),
        )?;
        // When gitlab doesn't find an ID, it simply doesn't list it. So we need to actually check
        // which nodes remain at the end to delete their DB entry.
        let mut node_ids: HashSet<&str> = node_ids.iter().map(|s| s.as_str()).collect();

        if let Some(data) = response.data {
            for node in &data.projects.nodes {
                if let Some(node) = node {
                    self.store_repository(
                        conn,
                        host,
                        &node.id,
                        &node.name_with_namespace,
                        &node.description,
                        &node.last_activity_at,
                        node.star_count,
                        node.forks_count,
                        node.open_issues_count.unwrap_or(0),
                    )?;
                    node_ids.remove(&node.id.as_str());
                }
            }
            if !response.errors.is_empty() {
                failure::bail!("error updating repositories: {:?}", response.errors);
            }

            // Those nodes were not returned by gitlab, meaning they don't exist (anymore?).
            for node in node_ids {
                self.delete_repository(conn, node, host)?;
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
    open_issues_count: Option<i64>,
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
                    GitLab::repository_name($url),
                    Some(RepositoryName {
                        owner: $owner,
                        repo: $repo,
                        host: $host,
                    })
                );
            };
            ($url:expr => None) => {
                assert_eq!(GitLab::repository_name($url), None);
            };
        }

        assert_name!("https://gitlab.com/onur/cratesfyi" => ("onur", "cratesfyi", "gitlab.com"));
        assert_name!("http://gitlab.com/onur/cratesfyi" => ("onur", "cratesfyi", "gitlab.com"));
        assert_name!("https://gitlab.com/onur/cratesfyi.git" => ("onur", "cratesfyi", "gitlab.com"));
        assert_name!("https://gitlab.com/docopt/docopt.rs" => ("docopt", "docopt.rs", "gitlab.com"));
        assert_name!("https://gitlab.com/onur23cmD_M_R_L_/crates_fy-i" => (
            "onur23cmD_M_R_L_", "crates_fy-i", "gitlab.com"
        ));
        assert_name!("https://gitlab.freedesktop.org/test/test" => (
            "test", "test", "gitlab.freedesktop.org"
        ));
        assert_name!("https://www.github.com/onur/cratesfyi" => None);
        assert_name!("https://github.com/onur/cratesfyi" => None);
    }
}
