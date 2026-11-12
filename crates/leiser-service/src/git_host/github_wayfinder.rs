//! Polling a repository's wayfinder maps on GitHub with `gh api`: the open
//! issues labelled `wayfinder:map`, each map's sub-issues, and the issues
//! blocking each ticket (GitHub's native issue dependencies). Every request
//! is conditional (`If-None-Match`), so a part that has not changed costs a
//! `304 Not Modified` answer and is taken from the last poll, whose memo
//! keeps the part's answer, cut down to what the harness reads.
//!
//! Lists are read page by page of [`PER_PAGE`] until a page is not full. A
//! ticket's blockers are asked for only when GitHub's summary of the
//! ticket says it has any.

use serde::de::DeserializeOwned;
use serde::de::IntoDeserializer;
use serde::de::value::{Error as ValueError, StrDeserializer};
use serde::{Deserialize, Serialize};

use super::github_poll::{Fetched, Requests};
use super::{
    Blocker, GhGitHost, GitHostError, HostAccountAccess, MapIssue, PolledWayfinderMaps,
    TicketIssue, https_repository,
};
use crate::{IssueRef, TicketType};

/// The label of a map issue.
const MAP_LABEL: &str = "wayfinder:map";
/// The start of a ticket's type label, such as `wayfinder:research`.
const TYPE_LABEL: &str = "wayfinder:";
/// The length of one page of a list.
const PER_PAGE: usize = 100;

#[derive(Deserialize, Serialize)]
struct MapJson {
    number: u64,
    title: String,
    html_url: String,
    /// Set on pull requests, which GitHub lists with issues.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pull_request: Option<PullRequestMarkerJson>,
}

#[derive(Deserialize, Serialize)]
struct PullRequestMarkerJson {}

#[derive(Deserialize, Serialize)]
struct TicketJson {
    number: u64,
    title: String,
    html_url: String,
    state: IssueStateJson,
    #[serde(default)]
    labels: Vec<LabelJson>,
    #[serde(default)]
    assignees: Vec<UserJson>,
    /// Missing where GitHub has no issue dependencies.
    #[serde(default)]
    issue_dependencies_summary: Option<DependenciesJson>,
}

#[derive(Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum IssueStateJson {
    Open,
    Closed,
}

#[derive(Deserialize, Serialize)]
struct LabelJson {
    name: String,
}

#[derive(Deserialize, Serialize)]
struct UserJson {
    login: String,
}

#[derive(Deserialize, Serialize)]
struct DependenciesJson {
    /// Open and closed blockers.
    total_blocked_by: u64,
}

#[derive(Deserialize, Serialize)]
struct BlockerJson {
    number: u64,
    state: IssueStateJson,
    /// Such as `https://api.github.com/repos/owner/name`.
    repository_url: String,
}

impl GhGitHost {
    /// Polls the open wayfinder maps of the repository at `remote`.
    pub(super) async fn poll_maps(
        &self,
        access: &HostAccountAccess,
        remote: &str,
        last: Option<&PolledWayfinderMaps>,
    ) -> Result<PolledWayfinderMaps, GitHostError> {
        let repository = https_repository(remote, &access.git_host)?;
        let mut requests = Requests::new(self, access, &repository, last.map(|last| &last.memo));
        let listed: Vec<MapJson> = all_pages(
            &mut requests,
            &format!("/issues?labels={MAP_LABEL}&state=open"),
        )
        .await?;
        let mut maps = Vec::new();
        for map in listed.into_iter().filter(|map| map.pull_request.is_none()) {
            let tickets: Vec<TicketJson> = all_pages(
                &mut requests,
                &format!("/issues/{}/sub_issues?", map.number),
            )
            .await?;
            let mut issues = Vec::new();
            for ticket in tickets {
                let blocked = ticket
                    .issue_dependencies_summary
                    .as_ref()
                    .is_none_or(|summary| summary.total_blocked_by > 0);
                let blocked_by = if blocked {
                    all_pages::<BlockerJson>(
                        &mut requests,
                        &format!("/issues/{}/dependencies/blocked_by?", ticket.number),
                    )
                    .await?
                    .into_iter()
                    .map(|blocker| blocker.read(&repository))
                    .collect()
                } else {
                    Vec::new()
                };
                issues.push(ticket.read(blocked_by));
            }
            maps.push(MapIssue {
                number: map.number,
                title: map.title,
                url: map.html_url,
                tickets: issues,
            });
        }
        Ok(PolledWayfinderMaps {
            maps,
            memo: requests.finish(),
        })
    }
}

/// Every item of the list at `query` (a path ending in its query string,
/// or in `?`), page by page until a page is not full.
async fn all_pages<T: DeserializeOwned + Serialize>(
    requests: &mut Requests<'_>,
    query: &str,
) -> Result<Vec<T>, GitHostError> {
    let separator = if query.ends_with('?') { "" } else { "&" };
    let mut items = Vec::new();
    for page in 1.. {
        let path = format!("{query}{separator}per_page={PER_PAGE}&page={page}");
        let found: Vec<T> = part(requests, &path).await?;
        let full = found.len() == PER_PAGE;
        items.extend(found);
        if !full {
            break;
        }
    }
    Ok(items)
}

/// One part of the maps: fresh from the git host, or the last poll's
/// answer when the git host answers that it has not changed.
async fn part<T: DeserializeOwned + Serialize>(
    requests: &mut Requests<'_>,
    path: &str,
) -> Result<T, GitHostError> {
    let unexpected = |error: String| GitHostError::UnexpectedAnswer {
        answer: format!("{path}: {error}"),
    };
    let kept = match requests.fetch(path).await? {
        Fetched::Unchanged => requests
            .last()
            .and_then(|last| last.answer(path))
            .ok_or_else(|| unexpected("304 without a kept answer".to_owned()))?
            .to_owned(),
        Fetched::Fresh(body) => {
            let part: T =
                serde_json::from_str(&body).map_err(|error| unexpected(error.to_string()))?;
            // Kept cut down to the fields above, not GitHub's whole answer
            // with every issue's body.
            serde_json::to_string(&part).map_err(|error| unexpected(error.to_string()))?
        }
    };
    if requests.memo().get(path).is_some() {
        requests.memo_mut().keep_answer(path, &kept);
    }
    serde_json::from_str(&kept).map_err(|error| unexpected(error.to_string()))
}

impl TicketJson {
    fn read(self, blocked_by: Vec<Blocker>) -> TicketIssue {
        TicketIssue {
            ticket_type: self
                .labels
                .iter()
                .find_map(|label| ticket_type_of_label(&label.name)),
            number: self.number,
            title: self.title,
            url: self.html_url,
            open: self.state == IssueStateJson::Open,
            assignees: self.assignees.into_iter().map(|user| user.login).collect(),
            blocked_by,
        }
    }
}

impl BlockerJson {
    /// The blocker, as an issue of `repository` (`owner/name`) or of
    /// another one.
    fn read(self, repository: &str) -> Blocker {
        let blocker_repository = self
            .repository_url
            .rsplit_once("/repos/")
            .map_or(self.repository_url.as_str(), |(_, repository)| repository)
            .to_owned();
        let issue = if blocker_repository.eq_ignore_ascii_case(repository) {
            IssueRef::Here {
                number: self.number,
            }
        } else {
            IssueRef::Elsewhere {
                repository: blocker_repository,
                number: self.number,
            }
        };
        Blocker {
            issue,
            open: self.state == IssueStateJson::Open,
        }
    }
}

/// The ticket type a label such as `wayfinder:research` names, by the
/// type's own name in the protocol.
fn ticket_type_of_label(label: &str) -> Option<TicketType> {
    let name: StrDeserializer<'_, ValueError> = label.strip_prefix(TYPE_LABEL)?.into_deserializer();
    TicketType::deserialize(name).ok()
}
