use std::sync::Arc;

use askama::Template;
use axum::{extract::Query, http::HeaderValue, response::IntoResponse, Extension};

use crate::{
    git::FormattedDiff,
    http, into_response,
    methods::{
        filters,
        repo::{commit::UriQuery, Repository, RepositoryPath, Result},
    },
    Git, ResponseEither,
};

#[derive(Template)]
#[template(path = "repo/diff.html")]
pub struct View {
    pub repo: Repository,
    pub diff: Arc<FormattedDiff>,
    pub branch: Option<Arc<str>>,
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

    let oid0 = match query.diff_base {
        Some(id0) => Some(open_repo.clone().oid(&id0)?),
        None => None, // later falls back to comparing to previous commit if oid0 is None.
    };

    let oid1 = match query.id {
        Some(id) => open_repo.clone().oid(&id)?,
        None => open_repo.clone().latest_commit_id().await?,
    };

    let diff = open_repo.diff(oid0, oid1).await?;

    if query.patch.is_some_and(|x| x == "1") {
        let headers = [(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain"),
        )];
        Ok(ResponseEither::Left(
            (headers, diff.diff_plain.clone()).into_response(),
        ))
    } else {
        Ok(ResponseEither::Right(into_response(View {
            repo,
            diff: Arc::new(diff),
            branch: query.branch,
        })))
    }
}
