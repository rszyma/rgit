use std::sync::Arc;

use askama::Template;
use axum::{
    extract::Query,
    response::{IntoResponse, Redirect},
    Extension,
};
use serde::Deserialize;

use crate::{
    git::{Commit, FormattedDiff},
    into_response,
    methods::{
        filters,
        repo::{Repository, RepositoryPath, Result},
    },
    Git, ResponseEither,
};

#[derive(Template)]
#[template(path = "repo/commit.html")]
pub struct View {
    pub repo: Repository,
    pub commit: Arc<Commit>,
    pub diff: Arc<FormattedDiff>,
    pub branch: Option<Arc<str>>,
    pub dl_branch: Arc<str>,
    pub id: Option<String>,
}

#[derive(Deserialize)]
pub struct UriQuery {
    pub id: Option<String>,
    #[serde(rename = "h")]
    pub branch: Option<Arc<str>>,
    #[serde(rename = "id0")]
    pub diff_base: Option<String>,
    // respond in plain text if set to 1 (only in diff and commit endpoints)
    pub patch: Option<String>,
}

pub async fn handle(
    Extension(repo): Extension<Repository>,
    Extension(RepositoryPath(repository_path)): Extension<RepositoryPath>,
    Extension(git): Extension<Arc<Git>>,
    Query(query): Query<UriQuery>,
) -> Result<impl IntoResponse> {
    let open_repo = git
        .clone()
        .repo(repository_path, query.branch.clone())
        .await?;

    let oid = match query.id {
        Some(ref id) => open_repo.clone().oid(id)?,
        None => open_repo.clone().latest_commit_id().await?,
    };

    let commit = open_repo.clone().commit(oid).await?;
    let diff = open_repo.diff(None, oid).await?;

    if query.diff_base.is_some() || query.patch.as_ref().is_some_and(|x| x == "1") {
        let cmp_to = query
            .id
            .clone()
            .or(query.branch.map(|x| x.to_string()))
            .unwrap_or_default(); // this should just err after redirect
        let id0 = match query.diff_base {
            Some(ref x) => format!("&id0={x}"),
            None => String::new(),
        };
        let patch = match query.patch {
            Some(ref x) => format!("&patch={x}"),
            None => String::new(),
        };
        let loc = format!("/{}/diff?id={cmp_to}{id0}{patch}", repo.display());
        return Ok(ResponseEither::Left(Redirect::to(&loc)));
    }

    Ok(ResponseEither::Right(into_response(View {
        repo,
        commit,
        diff: Arc::from(diff),
        branch: query.branch,
        id: query.id,
        dl_branch: "".into(), // disabled for now
    })))
}
