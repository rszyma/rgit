use std::{
    borrow::Cow,
    ffi::OsStr,
    fmt,
    fmt::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use axum::response::IntoResponse;
use bytes::{BufMut, Bytes, BytesMut};
use comrak::{ComrakPlugins, Options};
use git2::{
    DiffFormat, DiffLineType, DiffOptions, DiffStatsFormat, Email, EmailCreateOptions, ObjectType,
    Oid, Signature, TreeWalkResult,
};
use itertools::Itertools;
use moka::future::Cache;
use parking_lot::Mutex;
use syntect::{
    parsing::SyntaxSet,
    parsing::{BasicScopeStackOp, ParseState, Scope, ScopeStack, SCOPE_REPO},
    util::LinesWithEndings,
};
use time::OffsetDateTime;
use tracing::{error, instrument, warn};

use crate::syntax_highlight::ComrakSyntectAdapter;

type ReadmeCacheKey = (PathBuf, Option<Arc<str>>);

pub struct Git {
    commits: Cache<Oid, Arc<Commit>>,
    diffs: Cache<(Oid, Oid), Arc<FormattedDiff>>, // todo
    readme_cache: Cache<ReadmeCacheKey, Option<(ReadmeFormat, Arc<str>)>>,
    syntax_set: SyntaxSet,
}

impl Git {
    #[instrument(skip(syntax_set))]
    pub fn new(syntax_set: SyntaxSet) -> Self {
        Self {
            commits: Cache::builder()
                .time_to_live(Duration::from_secs(10))
                .max_capacity(100)
                .build(),
            diffs: Cache::builder()
                .time_to_live(Duration::from_secs(10))
                .max_capacity(100)
                .build(),
            readme_cache: Cache::builder()
                .time_to_live(Duration::from_secs(10))
                .max_capacity(100)
                .build(),
            syntax_set,
        }
    }
}

impl Git {
    #[instrument(skip(self))]
    pub async fn repo(
        self: Arc<Self>,
        repo_path: PathBuf,
        branch: Option<Arc<str>>,
    ) -> Result<Arc<OpenRepository>> {
        let repo = tokio::task::spawn_blocking({
            let repo_path = repo_path.clone();
            move || git2::Repository::open(repo_path)
        })
        .await
        .context("Failed to join Tokio task")?
        .map_err(|err| {
            error!("{}", err);
            anyhow!("Failed to open repository")
        })?;

        Ok(Arc::new(OpenRepository {
            git: self,
            cache_key: repo_path,
            repo: Mutex::new(repo),
            branch,
        }))
    }
}

pub struct OpenRepository {
    git: Arc<Git>,
    cache_key: PathBuf,
    repo: Mutex<git2::Repository>,
    branch: Option<Arc<str>>,
}

impl OpenRepository {
    pub async fn path(
        self: Arc<Self>,
        path: Option<PathBuf>,
        tree_id: Option<&str>,
        formatted: bool,
    ) -> Result<PathDestination> {
        let tree_id = tree_id
            .map(Oid::from_str)
            .transpose()
            .context("Failed to parse tree hash")?;

        tokio::task::spawn_blocking(move || {
            let repo = self.repo.lock();

            let mut tree = if let Some(tree_id) = tree_id {
                repo.find_tree(tree_id)
                    .context("Couldn't find tree with given id")?
            } else if let Some(branch) = &self.branch {
                let reference = repo.resolve_reference_from_short_name(branch)?;
                reference
                    .peel_to_tree()
                    .context("Couldn't find tree for reference")?
            } else {
                let head = repo.head().context("Failed to find HEAD")?;
                head.peel_to_tree()
                    .context("Couldn't find tree from HEAD")?
            };

            if let Some(path) = path.as_ref() {
                let item = tree.get_path(path).context("Path doesn't exist in tree")?;
                let object = item
                    .to_object(&repo)
                    .context("Path in tree isn't an object")?;

                if let Some(blob) = object.as_blob() {
                    // TODO: use Path here instead of a lossy utf8 conv
                    let name = String::from_utf8_lossy(item.name_bytes());
                    let path = path.clone().join(&*name);

                    let extension = path
                        .extension()
                        .or_else(|| path.file_name())
                        .map_or_else(|| Cow::Borrowed(""), OsStr::to_string_lossy);
                    let content = match (formatted, blob.is_binary()) {
                        (true, true) => Content::Binary(vec![]),
                        (true, false) => Content::Text(
                            format_file(
                                &String::from_utf8_lossy(blob.content()),
                                &extension,
                                &self.git.syntax_set,
                            )?
                            .into(),
                        ),
                        (false, true) => Content::Binary(blob.content().to_vec()),
                        (false, false) => Content::Text(
                            String::from_utf8_lossy(blob.content()).to_string().into(),
                        ),
                    };

                    return Ok(PathDestination::File(FileWithContent {
                        metadata: File {
                            mode: item.filemode(),
                            size: blob.size(),
                            path,
                            name: name.into_owned(),
                        },
                        content,
                    }));
                } else if let Ok(new_tree) = object.into_tree() {
                    tree = new_tree;
                } else {
                    anyhow::bail!("Given path not tree nor blob... what is it?!");
                }
            }

            let mut tree_items = Vec::new();

            for item in &tree {
                let object = item
                    .to_object(&repo)
                    .context("Expected item in tree to be object but it wasn't")?;

                let name = String::from_utf8_lossy(item.name_bytes()).into_owned();
                let path = path.clone().unwrap_or_default().join(&name);

                if let Some(blob) = object.as_blob() {
                    tree_items.push(TreeItem::File(File {
                        mode: item.filemode(),
                        size: blob.size(),
                        path,
                        name,
                    }));
                } else if let Some(_tree) = object.as_tree() {
                    tree_items.push(TreeItem::Tree(Tree {
                        mode: item.filemode(),
                        path,
                        name,
                    }));
                }
            }

            Ok(PathDestination::Tree(tree_items))
        })
        .await
        .context("Failed to join Tokio task")?
    }

    #[instrument(skip(self))]
    pub async fn tag_info(self: Arc<Self>) -> Result<DetailedTag> {
        tokio::task::spawn_blocking(move || {
            let tag_name = self.branch.clone().context("no tag given")?;
            let repo = self.repo.lock();

            let tag = repo
                .find_reference(&format!("refs/tags/{tag_name}"))
                .context("Given tag does not exist in repository")?
                .peel_to_tag()
                .context("Couldn't get to a tag from the given reference")?;
            let tag_target = tag.target().context("Couldn't find tagged object")?;

            let tagged_object = match tag_target.kind() {
                Some(ObjectType::Commit) => Some(TaggedObject::Commit(tag_target.id().to_string())),
                Some(ObjectType::Tree) => Some(TaggedObject::Tree(tag_target.id().to_string())),
                None | Some(_) => None,
            };

            Ok(DetailedTag {
                name: tag_name,
                tagger: tag.tagger().map(TryInto::try_into).transpose()?,
                message: tag
                    .message_bytes()
                    .map_or_else(|| Cow::Borrowed(""), String::from_utf8_lossy)
                    .into_owned(),
                tagged_object,
            })
        })
        .await
        .context("Failed to join Tokio task")?
    }

    #[instrument(skip(self))]
    pub async fn readme(
        self: Arc<Self>,
    ) -> Result<Option<(ReadmeFormat, Arc<str>)>, Arc<anyhow::Error>> {
        const README_FILES: &[&str] = &["README.md", "README", "README.txt", "readme.md"];

        let git = self.git.clone();

        git.readme_cache
            .try_get_with((self.cache_key.clone(), self.branch.clone()), async move {
                tokio::task::spawn_blocking(move || {
                    let repo = self.repo.lock();

                    let head = if let Some(reference) = &self.branch {
                        repo.resolve_reference_from_short_name(reference)?
                    } else {
                        repo.head().context("Couldn't find HEAD of repository")?
                    };

                    let commit = head.peel_to_commit().context(
                        "Couldn't find the commit that the HEAD of the repository refers to",
                    )?;
                    let tree = commit
                        .tree()
                        .context("Couldn't get the tree that the HEAD refers to")?;

                    for name in README_FILES {
                        let Some(tree_entry) = tree.get_name(name) else {
                            continue;
                        };

                        let Some(blob) = tree_entry
                            .to_object(&repo)
                            .ok()
                            .and_then(|v| v.into_blob().ok())
                        else {
                            continue;
                        };

                        let Ok(content) = std::str::from_utf8(blob.content()) else {
                            continue;
                        };

                        if Path::new(name).extension().and_then(OsStr::to_str) == Some("md") {
                            let value = parse_and_transform_markdown(content, &self.git.syntax_set);
                            return Ok(Some((ReadmeFormat::Markdown, Arc::from(value))));
                        }

                        return Ok(Some((ReadmeFormat::Plaintext, Arc::from(content))));
                    }

                    Ok(None)
                })
                .await
                .context("Failed to join Tokio task")?
            })
            .await
    }

    #[instrument(skip_all)]
    pub async fn archive(
        self: Arc<Self>,
        res: tokio::sync::mpsc::Sender<Result<Bytes, anyhow::Error>>,
        cont: tokio::sync::oneshot::Sender<()>,
        commit: Option<&str>,
    ) -> Result<(), anyhow::Error> {
        const BUFFER_CAP: usize = 512 * 1024;

        let commit = commit
            .map(Oid::from_str)
            .transpose()
            .context("failed to build oid")?;

        tokio::task::spawn_blocking(move || {
            let buffer = BytesMut::with_capacity(BUFFER_CAP + 1024);

            let flate = flate2::write::GzEncoder::new(buffer.writer(), flate2::Compression::fast());
            let mut archive = tar::Builder::new(flate);

            let repo = self.repo.lock();

            let tree = if let Some(commit) = commit {
                repo.find_commit(commit)?.tree()?
            } else if let Some(reference) = &self.branch {
                repo.resolve_reference_from_short_name(reference)?
                    .peel_to_tree()?
            } else {
                repo.head()
                    .context("Couldn't find HEAD of repository")?
                    .peel_to_tree()?
            };

            // tell the web server it can send response headers to the requester
            if cont.send(()).is_err() {
                return Err(anyhow!("requester gone"));
            }

            let mut callback = |root: &str, entry: &git2::TreeEntry| -> TreeWalkResult {
                if let Ok(blob) = entry.to_object(&repo).unwrap().peel_to_blob() {
                    let path =
                        Path::new(root).join(String::from_utf8_lossy(entry.name_bytes()).as_ref());

                    let mut header = tar::Header::new_gnu();
                    if let Err(error) = header.set_path(&path) {
                        warn!(%error, "Attempted to write invalid path to archive");
                        return TreeWalkResult::Skip;
                    }
                    header.set_size(blob.size() as u64);
                    #[allow(clippy::cast_sign_loss)]
                    header.set_mode(entry.filemode() as u32);
                    header.set_cksum();

                    if let Err(error) = archive.append(&header, blob.content()) {
                        error!(%error, "Failed to write blob to archive");
                        return TreeWalkResult::Abort;
                    }
                }

                if archive.get_ref().get_ref().get_ref().len() >= BUFFER_CAP {
                    let b = archive.get_mut().get_mut().get_mut().split().freeze();
                    if let Err(error) = res.blocking_send(Ok(b)) {
                        error!(%error, "Failed to send buffer to client");
                        return TreeWalkResult::Abort;
                    }
                }

                TreeWalkResult::Ok
            };

            tree.walk(git2::TreeWalkMode::PreOrder, &mut callback)?;

            res.blocking_send(Ok(archive.into_inner()?.finish()?.into_inner().freeze()))?;

            Ok::<_, anyhow::Error>(())
        })
        .await??;

        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn latest_commit_id(self: Arc<Self>) -> Result<Oid, Arc<anyhow::Error>> {
        tokio::task::spawn_blocking(move || {
            let repo = self.repo.lock();

            let head = if let Some(reference) = &self.branch {
                repo.resolve_reference_from_short_name(reference)
                    .map_err(anyhow::Error::from)?
            } else {
                repo.head().context("Couldn't find HEAD of repository")?
            };

            let commit = head
                .peel_to_commit()
                .context("Couldn't find commit HEAD of repository refers to")?;

            Ok(commit.id())
        })
        .await
        .context("Failed to join Tokio task")?
    }

    #[instrument(skip(self))]
    pub async fn commit(self: Arc<Self>, oid: Oid) -> Result<Arc<Commit>, Arc<anyhow::Error>> {
        let git = self.git.clone();
        git.commits
            .try_get_with(oid, async move {
                tokio::task::spawn_blocking(move || {
                    let repo = self.repo.lock();
                    let commit = repo.find_commit(oid)?;
                    let commit = Commit::try_from(commit)?;
                    Ok(Arc::new(commit))
                })
                .await
                .context("Failed to join Tokio task")?
            })
            .await
    }

    #[instrument(skip(self))]
    pub fn oid(&self, ref_: &str) -> Result<Oid, anyhow::Error> {
        let oid: Oid = Oid::from_str(ref_)
            .or_else(|_| {
                let repo = self.repo.lock();
                let resolved_ref = repo
                    .resolve_reference_from_short_name(ref_)
                    .map_err(anyhow::Error::from)?;
                resolved_ref
                    .peel_to_commit()
                    .context("Couldn't find commit from given ref")
                    .map(|x| x.id())
            })
            .map_err(anyhow::Error::from)?;
        Ok(oid)
    }

    /// Returns diff between two refs. If ref0 is not provided, returns diff compared to parent.
    #[instrument(skip(self))]
    pub async fn diff(
        self: Arc<Self>,
        oid0: Option<Oid>,
        oid1: Oid,
    ) -> Result<FormattedDiff, anyhow::Error> {
        let repo = self.repo.lock();

        let commit1: &git2::Commit = &repo.find_commit(oid1).map_err(anyhow::Error::from)?;

        let commit0: git2::Commit = if let Some(oid0) = oid0 {
            repo.find_commit(oid0).map_err(anyhow::Error::from)?
        } else {
            match commit1.parents().next() {
                Some(x) => x,
                // No previous commit, so no diff to show too.
                None => return Ok(FormattedDiff::default()),
            }
        };

        // todo: cache this. cache by tuple key: (id0, id1)
        fetch_diff_and_stats(&repo, &commit0, commit1, &self.git.syntax_set)
    }
}

fn parse_and_transform_markdown(s: &str, syntax_set: &SyntaxSet) -> String {
    let mut plugins = ComrakPlugins::default();

    let highlighter = ComrakSyntectAdapter { syntax_set };
    plugins.render.codefence_syntax_highlighter = Some(&highlighter);

    // enable gfm extensions
    // https://github.github.com/gfm/
    let mut options = Options::default();
    options.extension.autolink = true;
    options.extension.footnotes = true;
    options.extension.strikethrough = true;
    options.extension.table = true;
    options.extension.tagfilter = true;
    options.extension.tasklist = true;

    comrak::markdown_to_html_with_plugins(s, &options, &plugins)
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ReadmeFormat {
    Markdown,
    Plaintext,
}

pub enum PathDestination {
    Tree(Vec<TreeItem>),
    File(FileWithContent),
}

pub enum TreeItem {
    Tree(Tree),
    File(File),
}

#[derive(Debug)]
pub struct Tree {
    pub mode: i32,
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug)]
pub struct File {
    pub mode: i32,
    pub size: usize,
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug)]
pub struct FileWithContent {
    pub metadata: File,
    pub content: Content,
}

#[derive(Debug)]
pub enum Content {
    Text(Cow<'static, str>),
    Binary(Vec<u8>),
}

impl IntoResponse for Content {
    fn into_response(self) -> axum::response::Response {
        use axum::http;

        match self {
            Self::Text(t) => {
                let headers = [(
                    http::header::CONTENT_TYPE,
                    http::HeaderValue::from_static("text/plain; charset=UTF-8"),
                )];

                (headers, t).into_response()
            }
            Self::Binary(b) => {
                let headers = [(
                    http::header::CONTENT_TYPE,
                    http::HeaderValue::from_static("application/octet-stream"),
                )];

                (headers, b).into_response()
            }
        }
    }
}

#[derive(Debug)]
pub enum TaggedObject {
    Commit(String),
    Tree(String),
}

#[derive(Debug)]
pub struct DetailedTag {
    pub name: Arc<str>,
    pub tagger: Option<CommitUser>,
    pub message: String,
    pub tagged_object: Option<TaggedObject>,
}

#[derive(Debug)]
pub struct CommitUser {
    name: String,
    email: String,
    time: OffsetDateTime,
}

impl TryFrom<Signature<'_>> for CommitUser {
    type Error = anyhow::Error;

    fn try_from(v: Signature<'_>) -> Result<Self> {
        Ok(CommitUser {
            name: String::from_utf8_lossy(v.name_bytes()).into_owned(),
            email: String::from_utf8_lossy(v.email_bytes()).into_owned(),
            time: OffsetDateTime::from_unix_timestamp(v.when().seconds())?,
        })
    }
}

impl CommitUser {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn email(&self) -> &str {
        &self.email
    }

    pub fn time(&self) -> OffsetDateTime {
        self.time
    }
}

#[derive(Debug)]
pub struct Commit {
    author: CommitUser,
    committer: CommitUser,
    oid: String,
    tree: String,
    parents: Vec<String>,
    summary: String,
    body: String,
}

impl TryFrom<git2::Commit<'_>> for Commit {
    type Error = anyhow::Error;

    fn try_from(commit: git2::Commit<'_>) -> Result<Self> {
        Ok(Commit {
            author: CommitUser::try_from(commit.author())?,
            committer: CommitUser::try_from(commit.committer())?,
            oid: commit.id().to_string(),
            tree: commit.tree_id().to_string(),
            parents: commit.parent_ids().map(|v| v.to_string()).collect(),
            summary: commit
                .summary_bytes()
                .map_or_else(|| Cow::Borrowed(""), String::from_utf8_lossy)
                .into_owned(),
            body: commit
                .body_bytes()
                .map_or_else(|| Cow::Borrowed(""), String::from_utf8_lossy)
                .into_owned(),
        })
    }
}

impl Commit {
    pub fn author(&self) -> &CommitUser {
        &self.author
    }

    pub fn committer(&self) -> &CommitUser {
        &self.committer
    }

    pub fn oid(&self) -> &str {
        &self.oid
    }

    pub fn tree(&self) -> &str {
        &self.tree
    }

    pub fn parents(&self) -> impl Iterator<Item = &str> {
        self.parents.iter().map(String::as_str)
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub fn body(&self) -> &str {
        &self.body
    }
}

#[derive(Debug, Default)]
pub struct FormattedDiff {
    pub diff_stats: String,
    pub diff: String,
    pub diff_plain: Bytes,
}

#[instrument(skip(repo, commit0, commit1, syntax_set))]
fn fetch_diff_and_stats(
    repo: &git2::Repository,
    commit0: &git2::Commit<'_>,
    commit1: &git2::Commit<'_>,
    syntax_set: &SyntaxSet,
) -> Result<FormattedDiff> {
    let base_tree = commit0
        .tree()
        .context("Couldn't get tree for base commit")?;
    let current_tree = commit1.tree().context("Couldn't get tree for the commit")?;

    let mut diff_opts = DiffOptions::new();
    let diff =
        repo.diff_tree_to_tree(Some(&base_tree), Some(&current_tree), Some(&mut diff_opts))?;

    let mut diff_plain = BytesMut::new();

    let commit1_id = &commit1.id();

    let email = if commit1.parent_ids().contains(&commit0.id()) {
        Email::from_diff(
            &diff,
            1,
            1,
            &commit1.id(),
            commit1.summary().unwrap_or(""),
            commit1.body().unwrap_or(""),
            &commit1.author(),
            &mut EmailCreateOptions::default(),
        )
    } else {
        Email::from_diff(
            &diff,
            1,
            1,
            &Oid::zero(),
            "",
            "",
            &Signature::new("diff", "git@diff", &commit1.time()).unwrap(),
            &mut EmailCreateOptions::default(),
        )
    }
    .context("Couldn't build diff for commit")?;

    diff_plain.extend_from_slice(email.as_slice());

    let diff_stats = diff
        .stats()?
        .to_buf(DiffStatsFormat::FULL, 80)?
        .as_str()
        .unwrap_or("")
        .to_string();

    Ok(FormattedDiff {
        diff_stats: format_diff_stats(&diff_stats, &commit1_id.to_string()),
        diff: format_diff(&diff, syntax_set)?,
        diff_plain: diff_plain.into(),
    })
}

fn format_file(content: &str, extension: &str, syntax_set: &SyntaxSet) -> Result<String> {
    let mut out = String::new();
    format_file_inner(&mut out, content, extension, syntax_set, true)?;
    Ok(out)
}

// TODO: this is in some serious need of refactoring
fn format_file_inner(
    out: &mut String,
    content: &str,
    extension: &str,
    syntax_set: &SyntaxSet,
    code_tag: bool,
) -> Result<()> {
    let syntax = syntax_set
        .find_syntax_by_extension(extension)
        .unwrap_or_else(|| syntax_set.find_syntax_plain_text());
    let mut parse_state = ParseState::new(syntax);

    let mut scope_stack = ScopeStack::new();
    let mut span_empty = false;
    let mut span_start = 0;
    let mut open_spans = Vec::new();

    for line in LinesWithEndings::from(content) {
        if code_tag {
            out.push_str("<code>");
        }

        if line.len() > 2048 {
            // avoid highlighting overly complex lines
            let line = if code_tag { line.trim_end() } else { line };
            write!(out, "{}", Escape(line))?;
        } else {
            let mut cur_index = 0;
            let ops = parse_state.parse_line(line, syntax_set)?;
            out.reserve(line.len() + ops.len() * 8);

            if code_tag {
                for scope in &open_spans {
                    out.push_str("<span class=\"");
                    scope_to_classes(out, *scope);
                    out.push_str("\">");
                }
            }

            // mostly copied from syntect, but slightly modified to keep track
            // of open spans, so we can open and close them for each line
            for &(i, ref op) in &ops {
                if i > cur_index {
                    let prefix = &line[cur_index..i];
                    let prefix = if code_tag {
                        prefix.trim_end_matches('\n')
                    } else {
                        prefix
                    };
                    write!(out, "{}", Escape(prefix))?;

                    span_empty = false;
                    cur_index = i;
                }

                scope_stack.apply_with_hook(op, |basic_op, _| match basic_op {
                    BasicScopeStackOp::Push(scope) => {
                        span_start = out.len();
                        span_empty = true;
                        out.push_str("<span class=\"");
                        open_spans.push(scope);
                        scope_to_classes(out, scope);
                        out.push_str("\">");
                    }
                    BasicScopeStackOp::Pop => {
                        open_spans.pop();
                        if span_empty {
                            out.truncate(span_start);
                        } else {
                            out.push_str("</span>");
                        }
                        span_empty = false;
                    }
                })?;
            }

            let line = if code_tag { line.trim_end() } else { line };
            if line.len() > cur_index {
                write!(out, "{}", Escape(&line[cur_index..]))?;
            }

            if code_tag {
                for _scope in &open_spans {
                    out.push_str("</span>");
                }
            }
        }

        if code_tag {
            out.push_str("</code>\n");
        }
    }

    if !code_tag {
        for _scope in &open_spans {
            out.push_str("</span>");
        }
    }

    Ok(())
}

fn scope_to_classes(s: &mut String, scope: Scope) {
    let repo = SCOPE_REPO.lock().unwrap();
    for i in 0..(scope.len()) {
        let atom = scope.atom_at(i as usize);
        let atom_s = repo.atom_str(atom);
        if i != 0 {
            s.push(' ');
        }
        s.push_str(atom_s);
    }
}

// Copied from syntect as it isn't exposed from there.
pub struct Escape<'a>(pub &'a str);

impl<'a> fmt::Display for Escape<'a> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Escape(s) = *self;
        let pile_o_bits = s;
        let mut last = 0;
        for (i, ch) in s.bytes().enumerate() {
            match ch as char {
                '<' | '>' | '&' | '\'' | '"' => {
                    fmt.write_str(&pile_o_bits[last..i])?;
                    let s = match ch as char {
                        '>' => "&gt;",
                        '<' => "&lt;",
                        '&' => "&amp;",
                        '\'' => "&#39;",
                        '"' => "&quot;",
                        _ => unreachable!(),
                    };
                    fmt.write_str(s)?;
                    last = i + 1;
                }
                _ => {}
            }
        }

        if last < s.len() {
            fmt.write_str(&pile_o_bits[last..])?;
        }
        Ok(())
    }
}

#[instrument(skip(diff, syntax_set))]
fn format_diff(diff: &git2::Diff<'_>, syntax_set: &SyntaxSet) -> Result<String> {
    let mut diff_output = String::new();

    diff.print(DiffFormat::Patch, |delta, _diff_hunk, diff_line| {
        let (class, should_highlight_as_source) = match diff_line.origin_value() {
            DiffLineType::Addition => (Some("add-line"), true),
            DiffLineType::Deletion => (Some("remove-line"), true),
            DiffLineType::Context => (Some("context"), true),
            DiffLineType::AddEOFNL => (Some("remove-line"), false),
            DiffLineType::DeleteEOFNL => (Some("add-line"), false),
            DiffLineType::FileHeader => (Some("file-header"), false),
            _ => (None, false),
        };

        let line = String::from_utf8_lossy(diff_line.content());

        let extension = if should_highlight_as_source {
            if let Some(path) = delta.new_file().path() {
                path.extension()
                    .or_else(|| path.file_name())
                    .map_or_else(|| Cow::Borrowed(""), OsStr::to_string_lossy)
            } else {
                Cow::Borrowed("")
            }
        } else {
            Cow::Borrowed("patch")
        };

        if let Some(class) = class {
            let _ = write!(diff_output, r#"<span class="diff-{class}">"#);
        }

        let _res = format_file_inner(&mut diff_output, &line, &extension, syntax_set, false);

        if class.is_some() {
            diff_output.push_str("</span>");
        }

        true
    })
    .context("Failed to prepare diff")?;

    Ok(diff_output)
}

fn format_diff_stats(diff_stats: &str, ref_: &str) -> String {
    diff_stats
        .split('\n')
        .map(|line| {
            let Some((left, right)) = line.split_once('|') else {
                return line.to_owned();
            };
            let filepath = left.trim();
            let htmled_filepath =
                format!(r#"<a href="../tree/{filepath}?id={ref_}">{filepath}</a>"#);
            let spaces_padding = " ".repeat(left.len() - filepath.len());
            format!("{htmled_filepath}{spaces_padding}|{right}")
        })
        .join("\n")
}
