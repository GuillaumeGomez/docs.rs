use crate::error::Result;
use chrono::{DateTime, Utc};
use log::warn;
use reqwest::{
    blocking::Client as HttpClient,
    header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT},
};
use serde::Deserialize;
use std::collections::HashSet;

use crate::repositories::{
    FetchRepositoriesResult, Repository, RepositoryForge, RepositoryName, APP_USER_AGENT,
};

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

pub struct GitLab {
    client: HttpClient,
    host: &'static str,
}

impl GitLab {
    pub fn new(host: &'static str, access_token: &Option<String>) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(APP_USER_AGENT));
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));

        if let Some(token) = access_token {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {}", token))?,
            );
        } else {
            warn!(
                "will try to retrieve `{}` stats without token since none was provided",
                host
            );
        }

        let client = HttpClient::builder().default_headers(headers).build()?;
        Ok(GitLab { client, host })
    }
}

impl RepositoryForge for GitLab {
    fn host(&self) -> &str {
        self.host
    }

    fn icon(&self) -> &'static str {
        "gitlab"
    }

    fn chunk_size(&self) -> usize {
        5
    }

    fn fetch_repository(&self, name: &RepositoryName) -> Result<Option<Repository>> {
        let project_path = format!("{}/{}", name.owner, name.repo);
        // Fetch the latest information from the Gitlab API.
        let response: GraphResponse<GraphProjectNode> = self.graphql(
            GRAPHQL_SINGLE,
            serde_json::json!({
                "fullPath": &project_path,
            }),
        )?;
        if let Some(repo) = response.data.and_then(|d| d.project) {
            Ok(Some(Repository {
                id: repo.id,
                name_with_owner: repo.name_with_namespace,
                description: repo.description,
                last_activity_at: repo.last_activity_at,
                stars: repo.star_count,
                forks: repo.forks_count,
                issues: repo.open_issues_count.unwrap_or(0),
            }))
        } else {
            Ok(None)
        }
    }

    fn fetch_repositories(&self, ids: &[String]) -> Result<FetchRepositoriesResult> {
        let response: GraphResponse<GraphProjects<Option<GraphProject>>> = self.graphql(
            GRAPHQL_UPDATE,
            serde_json::json!({
                "ids": ids,
            }),
        )?;
        let mut ret = FetchRepositoriesResult::default();
        // When gitlab doesn't find an ID, it simply doesn't list it. So we need to actually check
        // which nodes remain at the end to delete their DB entry.
        let mut node_ids: HashSet<&String> = ids.iter().collect();

        if let Some(data) = response.data {
            if !response.errors.is_empty() {
                failure::bail!("error updating repositories: {:?}", response.errors);
            }
            for node in data.projects.nodes.into_iter() {
                if let Some(node) = node {
                    let repo = Repository {
                        id: node.id,
                        name_with_owner: node.name_with_namespace,
                        description: node.description,
                        last_activity_at: node.last_activity_at,
                        stars: node.star_count,
                        forks: node.forks_count,
                        issues: node.open_issues_count.unwrap_or(0),
                    };
                    let id = repo.id.clone();
                    node_ids.remove(&id);
                    ret.present.insert(id, repo);
                }
            }

            // Those nodes were not returned by gitlab, meaning they don't exist (anymore?).
            ret.missing = node_ids.into_iter().map(|s| s.to_owned()).collect();

            Ok(ret)
        } else {
            failure::bail!("no data")
        }
    }
}

impl GitLab {
    fn graphql<T: serde::de::DeserializeOwned + std::fmt::Debug>(
        &self,
        query: &str,
        variables: impl serde::Serialize,
    ) -> Result<GraphResponse<T>> {
        Ok(self
            .client
            .post(&format!("https://{}/api/graphql", self.host))
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
