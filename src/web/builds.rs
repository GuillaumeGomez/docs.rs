use super::{
    cache::CachePolicy,
    error::{AxumNope, JsonAxumNope, JsonAxumResult},
    headers::CanonicalUrl,
};
use crate::{
    db::types::BuildStatus,
    docbuilder::Limits,
    impl_axum_webpage,
    utils::spawn_blocking,
    web::{
        crate_details::CrateDetails,
        error::AxumResult,
        extractors::{DbConnection, Path},
        match_version, MetaData, ReqVersion,
    },
    BuildQueue, Config,
};
use anyhow::{anyhow, Result};
use axum::{
    extract::Extension, http::header::ACCESS_CONTROL_ALLOW_ORIGIN, response::IntoResponse, Json,
};
use axum_extra::{
    headers::{authorization::Bearer, Authorization},
    TypedHeader,
};
use chrono::{DateTime, Utc};
use http::StatusCode;
use semver::Version;
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Build {
    id: i32,
    rustc_version: Option<String>,
    docsrs_version: Option<String>,
    build_status: BuildStatus,
    build_time: Option<DateTime<Utc>>,
    errors: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct BuildsPage {
    metadata: MetaData,
    builds: Vec<Build>,
    limits: Limits,
    canonical_url: CanonicalUrl,
    use_direct_platform_links: bool,
}

impl_axum_webpage! {
    BuildsPage = "crate/builds.html",
}

pub(crate) async fn build_list_handler(
    Path((name, req_version)): Path<(String, ReqVersion)>,
    mut conn: DbConnection,
    Extension(config): Extension<Arc<Config>>,
) -> AxumResult<impl IntoResponse> {
    let version = match_version(&mut conn, &name, &req_version)
        .await?
        .assume_exact_name()?
        .into_canonical_req_version_or_else(|version| {
            AxumNope::Redirect(
                format!("/crate/{name}/{version}/builds"),
                CachePolicy::ForeverInCdn,
            )
        })?
        .into_version();

    Ok(BuildsPage {
        metadata: MetaData::from_crate(&mut conn, &name, &version, Some(req_version)).await?,
        builds: get_builds(&mut conn, &name, &version).await?,
        limits: Limits::for_crate(&config, &mut conn, &name).await?,
        canonical_url: CanonicalUrl::from_path(format!("/crate/{name}/latest/builds")),
        use_direct_platform_links: true,
    }
    .into_response())
}

pub(crate) async fn build_list_json_handler(
    Path((name, req_version)): Path<(String, ReqVersion)>,
    mut conn: DbConnection,
) -> AxumResult<impl IntoResponse> {
    let version = match_version(&mut conn, &name, &req_version)
        .await?
        .assume_exact_name()?
        .into_canonical_req_version_or_else(|version| {
            AxumNope::Redirect(
                format!("/crate/{name}/{version}/builds.json"),
                CachePolicy::ForeverInCdn,
            )
        })?
        .into_version();

    Ok((
        Extension(CachePolicy::NoStoreMustRevalidate),
        [(ACCESS_CONTROL_ALLOW_ORIGIN, "*")],
        Json(
            get_builds(&mut conn, &name, &version)
                .await?
                .iter()
                .map(|build| {
                    // for backwards compatibility in this API, we
                    // * convert the build status to a boolean
                    // * already filter out in-progress builds
                    //
                    // even when we start showing in-progress builds in the UI,
                    // we might still not show them here for backwards
                    // compatibility.
                    serde_json::json!({
                        "id": build.id,
                        "rustc_version": build.rustc_version,
                        "docsrs_version": build.docsrs_version,
                        "build_status": build.build_status.is_success(),
                        "build_time": build.build_time,
                    })
                })
                .collect::<Vec<_>>(),
        ),
    )
        .into_response())
}

async fn build_trigger_check(
    mut conn: DbConnection,
    name: &String,
    version: &Version,
    build_queue: &Arc<BuildQueue>,
) -> AxumResult<impl IntoResponse> {
    let _ = CrateDetails::new(&mut *conn, &name, &version, None, vec![])
        .await?
        .ok_or(AxumNope::VersionNotFound)?;

    let crate_version_is_in_queue = spawn_blocking({
        let name = name.clone();
        let version_string = version.to_string();
        let build_queue = build_queue.clone();
        move || build_queue.has_build_queued(&name, &version_string)
    })
    .await?;
    if crate_version_is_in_queue {
        return Err(AxumNope::BadRequest(anyhow!(
            "crate {name} {version} already queued for rebuild"
        )));
    }

    Ok(())
}

// Priority according to issue #2442; positive here as it's inverted.
// FUTURE: move to a crate-global enum with all special priorities?
const TRIGGERED_REBUILD_PRIORITY: i32 = 5;

pub(crate) async fn build_trigger_rebuild_handler(
    Path((name, version)): Path<(String, Version)>,
    conn: DbConnection,
    Extension(build_queue): Extension<Arc<BuildQueue>>,
    Extension(config): Extension<Arc<Config>>,
    opt_auth_header: Option<TypedHeader<Authorization<Bearer>>>,
) -> JsonAxumResult<impl IntoResponse> {
    let expected_token =
        config
            .cratesio_token
            .as_ref()
            .ok_or(JsonAxumNope(AxumNope::Unauthorized(
                "Endpoint is not configured",
            )))?;

    // (Future: would it be better to have standard middleware handle auth?)
    let TypedHeader(auth_header) = opt_auth_header.ok_or(JsonAxumNope(AxumNope::Unauthorized(
        "Missing authentication token",
    )))?;
    if auth_header.token() != expected_token {
        return Err(JsonAxumNope(AxumNope::Unauthorized(
            "The token used for authentication is not valid",
        )));
    }

    build_trigger_check(conn, &name, &version, &build_queue)
        .await
        .map_err(JsonAxumNope)?;

    spawn_blocking({
        let name = name.clone();
        let version_string = version.to_string();
        move || {
            build_queue.add_crate(
                &name,
                &version_string,
                TRIGGERED_REBUILD_PRIORITY,
                None, /* because crates.io is the only service that calls this endpoint */
            )
        }
    })
    .await
    .map_err(|e| JsonAxumNope(e.into()))?;

    Ok((StatusCode::CREATED, Json(json!({}))))
}

async fn get_builds(
    conn: &mut sqlx::PgConnection,
    name: &str,
    version: &Version,
) -> Result<Vec<Build>> {
    Ok(sqlx::query_as!(
        Build,
        r#"SELECT
            builds.id,
            builds.rustc_version,
            builds.docsrs_version,
            builds.build_status as "build_status: BuildStatus",
            builds.build_time,
            builds.errors
         FROM builds
         INNER JOIN releases ON releases.id = builds.rid
         INNER JOIN crates ON releases.crate_id = crates.id
         WHERE
            crates.name = $1 AND
            releases.version = $2 AND
            builds.build_status != 'in_progress'
         ORDER BY id DESC"#,
        name,
        version.to_string(),
    )
    .fetch_all(&mut *conn)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::BuildStatus;
    use crate::{
        test::{assert_cache_control, fake_release_that_failed_before_build, wrapper, FakeBuild},
        web::cache::CachePolicy,
    };
    use chrono::{DateTime, Duration, Utc};
    use kuchikiki::traits::TendrilSink;
    use reqwest::StatusCode;

    #[test]
    fn build_list_empty_build() {
        wrapper(|env| {
            env.runtime().block_on(async {
                let mut conn = env.async_db().await.async_conn().await;
                fake_release_that_failed_before_build(&mut conn, "foo", "0.1.0", "some errors")
                    .await
            })?;

            let response = env
                .frontend()
                .get("/crate/foo/0.1.0/builds")
                .send()?
                .error_for_status()?;
            assert_cache_control(&response, CachePolicy::NoCaching, &env.config());
            let page = kuchikiki::parse_html().one(response.text()?);

            let rows: Vec<_> = page
                .select("ul > li a.release")
                .unwrap()
                .map(|row| row.text_contents())
                .collect();

            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].chars().filter(|&c| c == '—').count(), 3);

            Ok(())
        });
    }

    #[test]
    fn build_list() {
        wrapper(|env| {
            env.fake_release()
                .name("foo")
                .version("0.1.0")
                .builds(vec![
                    FakeBuild::default()
                        .rustc_version("rustc (blabla 2019-01-01)")
                        .docsrs_version("docs.rs 1.0.0"),
                    FakeBuild::default()
                        .successful(false)
                        .rustc_version("rustc (blabla 2020-01-01)")
                        .docsrs_version("docs.rs 2.0.0"),
                    FakeBuild::default()
                        .rustc_version("rustc (blabla 2021-01-01)")
                        .docsrs_version("docs.rs 3.0.0"),
                    FakeBuild::default()
                        .build_status(BuildStatus::InProgress)
                        .rustc_version("rustc (blabla 2022-01-01)")
                        .docsrs_version("docs.rs 4.0.0"),
                ])
                .create()?;

            let response = env.frontend().get("/crate/foo/0.1.0/builds").send()?;
            assert_cache_control(&response, CachePolicy::NoCaching, &env.config());
            let page = kuchikiki::parse_html().one(response.text()?);

            let rows: Vec<_> = page
                .select("ul > li a.release")
                .unwrap()
                .map(|row| row.text_contents())
                .collect();

            assert!(rows[0].contains("rustc (blabla 2021-01-01)"));
            assert!(rows[0].contains("docs.rs 3.0.0"));
            assert!(rows[1].contains("rustc (blabla 2020-01-01)"));
            assert!(rows[1].contains("docs.rs 2.0.0"));
            assert!(rows[2].contains("rustc (blabla 2019-01-01)"));
            assert!(rows[2].contains("docs.rs 1.0.0"));

            Ok(())
        });
    }

    #[test]
    fn build_list_json() {
        wrapper(|env| {
            env.fake_release()
                .name("foo")
                .version("0.1.0")
                .builds(vec![
                    FakeBuild::default()
                        .rustc_version("rustc (blabla 2019-01-01)")
                        .docsrs_version("docs.rs 1.0.0"),
                    FakeBuild::default()
                        .successful(false)
                        .rustc_version("rustc (blabla 2020-01-01)")
                        .docsrs_version("docs.rs 2.0.0"),
                    FakeBuild::default()
                        .rustc_version("rustc (blabla 2021-01-01)")
                        .docsrs_version("docs.rs 3.0.0"),
                    FakeBuild::default()
                        .build_status(BuildStatus::InProgress)
                        .rustc_version("rustc (blabla 2022-01-01)")
                        .docsrs_version("docs.rs 4.0.0"),
                ])
                .create()?;

            let response = env.frontend().get("/crate/foo/0.1.0/builds.json").send()?;
            assert_cache_control(&response, CachePolicy::NoStoreMustRevalidate, &env.config());
            let value: serde_json::Value = serde_json::from_str(&response.text()?)?;

            assert_eq!(value.as_array().unwrap().len(), 3);

            assert_eq!(value.pointer("/0/build_status"), Some(&true.into()));
            assert_eq!(
                value.pointer("/0/docsrs_version"),
                Some(&"docs.rs 3.0.0".into())
            );
            assert_eq!(
                value.pointer("/0/rustc_version"),
                Some(&"rustc (blabla 2021-01-01)".into())
            );
            assert!(value.pointer("/0/id").unwrap().is_i64());
            assert!(serde_json::from_value::<DateTime<Utc>>(
                value.pointer("/0/build_time").unwrap().clone()
            )
            .is_ok());

            assert_eq!(value.pointer("/1/build_status"), Some(&false.into()));
            assert_eq!(
                value.pointer("/1/docsrs_version"),
                Some(&"docs.rs 2.0.0".into())
            );
            assert_eq!(
                value.pointer("/1/rustc_version"),
                Some(&"rustc (blabla 2020-01-01)".into())
            );
            assert!(value.pointer("/1/id").unwrap().is_i64());
            assert!(serde_json::from_value::<DateTime<Utc>>(
                value.pointer("/1/build_time").unwrap().clone()
            )
            .is_ok());

            assert_eq!(value.pointer("/2/build_status"), Some(&true.into()));
            assert_eq!(
                value.pointer("/2/docsrs_version"),
                Some(&"docs.rs 1.0.0".into())
            );
            assert_eq!(
                value.pointer("/2/rustc_version"),
                Some(&"rustc (blabla 2019-01-01)".into())
            );
            assert!(value.pointer("/2/id").unwrap().is_i64());
            assert!(serde_json::from_value::<DateTime<Utc>>(
                value.pointer("/2/build_time").unwrap().clone()
            )
            .is_ok());

            assert!(
                value.pointer("/1/build_time").unwrap().as_str().unwrap()
                    < value.pointer("/0/build_time").unwrap().as_str().unwrap()
            );
            assert!(
                value.pointer("/2/build_time").unwrap().as_str().unwrap()
                    < value.pointer("/1/build_time").unwrap().as_str().unwrap()
            );

            Ok(())
        });
    }

    #[test]
    fn build_trigger_rebuild_missing_config() {
        wrapper(|env| {
            env.fake_release().name("foo").version("0.1.0").create()?;

            {
                let response = env.frontend().get("/crate/regex/1.3.1/rebuild").send()?;
                // Needs POST
                assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
            }

            {
                let response = env.frontend().post("/crate/regex/1.3.1/rebuild").send()?;
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
                let json: serde_json::Value = response.json()?;
                assert_eq!(
                    json,
                    serde_json::json!({
                        "title": "Unauthorized",
                        "message": "Endpoint is not configured"
                    })
                );
            }

            Ok(())
        })
    }

    #[test]
    fn build_trigger_rebuild_with_config() {
        wrapper(|env| {
            let correct_token = "foo137";
            env.override_config(|config| config.cratesio_token = Some(correct_token.into()));

            env.fake_release().name("foo").version("0.1.0").create()?;

            {
                let response = env.frontend().post("/crate/regex/1.3.1/rebuild").send()?;
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
                let json: serde_json::Value = response.json()?;
                assert_eq!(
                    json,
                    serde_json::json!({
                        "title": "Unauthorized",
                        "message": "Missing authentication token"
                    })
                );
            }

            {
                let response = env
                    .frontend()
                    .post("/crate/regex/1.3.1/rebuild")
                    .bearer_auth("someinvalidtoken")
                    .send()?;
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
                let json: serde_json::Value = response.json()?;
                assert_eq!(
                    json,
                    serde_json::json!({
                        "title": "Unauthorized",
                        "message": "The token used for authentication is not valid"
                    })
                );
            }

            assert_eq!(env.build_queue().pending_count()?, 0);
            assert!(!env.build_queue().has_build_queued("foo", "0.1.0")?);

            {
                let response = env
                    .frontend()
                    .post("/crate/foo/0.1.0/rebuild")
                    .bearer_auth(correct_token)
                    .send()?;
                assert_eq!(response.status(), StatusCode::CREATED);
                let json: serde_json::Value = response.json()?;
                assert_eq!(json, serde_json::json!({}));
            }

            assert_eq!(env.build_queue().pending_count()?, 1);
            assert!(env.build_queue().has_build_queued("foo", "0.1.0")?);

            {
                let response = env
                    .frontend()
                    .post("/crate/foo/0.1.0/rebuild")
                    .bearer_auth(correct_token)
                    .send()?;
                assert_eq!(response.status(), StatusCode::BAD_REQUEST);
                let json: serde_json::Value = response.json()?;
                assert_eq!(
                    json,
                    serde_json::json!({
                        "title": "Bad request",
                        "message": "crate foo 0.1.0 already queued for rebuild"
                    })
                );
            }

            assert_eq!(env.build_queue().pending_count()?, 1);
            assert!(env.build_queue().has_build_queued("foo", "0.1.0")?);

            Ok(())
        });
    }

    #[test]
    fn build_empty_list() {
        wrapper(|env| {
            env.fake_release()
                .name("foo")
                .version("0.1.0")
                .no_builds()
                .create()?;

            let response = env.frontend().get("/crate/foo/0.1.0/builds").send()?;

            // FIXME: temporarily we don't show in-progress releases anywhere, which means we don't
            // show releases without builds anywhere.
            assert_eq!(response.status(), StatusCode::NOT_FOUND);

            // assert_cache_control(&response, CachePolicy::NoCaching, &env.config());
            // let page = kuchikiki::parse_html().one(response.text()?);

            // let rows: Vec<_> = page
            //     .select("ul > li a.release")
            //     .unwrap()
            //     .map(|row| row.text_contents())
            //     .collect();

            // assert!(rows.is_empty());

            // let warning = page
            //     .select_first(".warning")
            //     .expect("missing warning element")
            //     .text_contents();

            // assert!(warning.contains("has not built"));
            // assert!(warning.contains("queued"));
            // assert!(warning.contains("open an issue"));

            Ok(())
        });
    }

    #[test]
    fn limits() {
        wrapper(|env| {
            env.fake_release().name("foo").version("0.1.0").create()?;

            env.db().conn().query(
                "INSERT INTO sandbox_overrides
                    (crate_name, max_memory_bytes, timeout_seconds, max_targets)
                 VALUES ($1, $2, $3, $4)",
                &[
                    &"foo",
                    &(6 * 1024 * 1024 * 1024i64),
                    &(Duration::try_hours(2).unwrap().num_seconds() as i32),
                    &1,
                ],
            )?;

            let page = kuchikiki::parse_html().one(
                env.frontend()
                    .get("/crate/foo/0.1.0/builds")
                    .send()?
                    .text()?,
            );

            let header = page.select(".about h4").unwrap().next().unwrap();
            assert_eq!(header.text_contents(), "foo's sandbox limits");

            let values: Vec<_> = page
                .select(".about table tr td:last-child")
                .unwrap()
                .map(|row| row.text_contents())
                .collect();
            let values: Vec<_> = values.iter().map(|v| &**v).collect();

            dbg!(&values);
            assert!(values.contains(&"6 GB"));
            assert!(values.contains(&"2 hours"));
            assert!(values.contains(&"100 kB"));
            assert!(values.contains(&"blocked"));
            assert!(values.contains(&"1"));

            Ok(())
        });
    }

    #[test]
    fn latest_200() {
        wrapper(|env| {
            env.fake_release()
                .name("aquarelle")
                .version("0.1.0")
                .builds(vec![FakeBuild::default()
                    .rustc_version("rustc (blabla 2019-01-01)")
                    .docsrs_version("docs.rs 1.0.0")])
                .create()?;

            env.fake_release()
                .name("aquarelle")
                .version("0.2.0")
                .builds(vec![FakeBuild::default()
                    .rustc_version("rustc (blabla 2019-01-01)")
                    .docsrs_version("docs.rs 1.0.0")])
                .create()?;

            let resp = env
                .frontend()
                .get("/crate/aquarelle/latest/builds")
                .send()?;
            assert!(resp
                .url()
                .as_str()
                .ends_with("/crate/aquarelle/latest/builds"));
            let body = String::from_utf8(resp.bytes().unwrap().to_vec()).unwrap();
            assert!(body.contains("<a href=\"/crate/aquarelle/latest/features\""));
            assert!(body.contains("<a href=\"/crate/aquarelle/latest/builds\""));
            assert!(body.contains("<a href=\"/crate/aquarelle/latest/source/\""));
            assert!(body.contains("<a href=\"/crate/aquarelle/latest\""));

            let resp_json = env
                .frontend()
                .get("/crate/aquarelle/latest/builds.json")
                .send()?;
            assert!(resp_json
                .url()
                .as_str()
                .ends_with("/crate/aquarelle/latest/builds.json"));

            Ok(())
        });
    }

    #[test]
    fn crate_version_not_found() {
        wrapper(|env| {
            env.fake_release()
                .name("foo")
                .version("0.1.0")
                .builds(vec![FakeBuild::default()
                    .rustc_version("rustc (blabla 2019-01-01)")
                    .docsrs_version("docs.rs 1.0.0")])
                .create()?;

            let resp = env.frontend().get("/crate/foo/0.2.0/builds").send()?;
            dbg!(resp.url().as_str());
            assert!(resp.url().as_str().ends_with("/crate/foo/0.2.0/builds"));
            assert_eq!(resp.status(), StatusCode::NOT_FOUND);
            Ok(())
        });
    }

    #[test]
    fn invalid_semver() {
        wrapper(|env| {
            env.fake_release()
                .name("foo")
                .version("0.1.0")
                .builds(vec![FakeBuild::default()
                    .rustc_version("rustc (blabla 2019-01-01)")
                    .docsrs_version("docs.rs 1.0.0")])
                .create()?;

            let resp = env.frontend().get("/crate/foo/0,1,0/builds").send()?;
            dbg!(resp.url().as_str());
            assert!(resp.url().as_str().ends_with("/crate/foo/0,1,0/builds"));
            assert_eq!(resp.status(), StatusCode::NOT_FOUND);
            Ok(())
        });
    }
}
