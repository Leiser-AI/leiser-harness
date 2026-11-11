//! Polling a repository's wayfinder maps on GitHub with `gh api`: the open
//! issues labelled `wayfinder:map`, each map's sub-issues, and the issues
//! blocking each ticket (GitHub's native issue dependencies). Every request
//! is conditional (`If-None-Match`), so a part that has not changed costs a
//! `304 Not Modified` answer and is taken from the last poll, whose memo
//! keeps the part's answer, cut down to what the harness reads.
//!
//! Lists are read one page of [`PER_PAGE`]: a map with more tickets, or a
//! ticket with more blockers, shows the first ones only. A ticket's
//! blockers are asked for only when GitHub's summary of the ticket says it
//! has any.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::github_poll::Answer;
use super::{
    Blocker, GhGitHost, GitHostError, HostAccountAccess, IssueRef, MapIssue, PollMemo,
    PolledWayfinderMaps, TicketIssue, https_repository,
};
use crate::TicketKind;

/// The label of a map issue.
const MAP_LABEL: &str = "wayfinder:map";
/// The start of a ticket's type label, such as `wayfinder:research`.
const KIND_LABEL: &str = "wayfinder:";
/// The length of one page of a list.
const PER_PAGE: u64 = 100;
/// What the memo key of a part's kept answer adds to the part's path.
const ANSWER: &str = " answer";

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
        let mut poll = MapsPoll {
            host: self,
            access,
            repository: &repository,
            last: last.map(|last| &last.memo),
            memo: PollMemo::default(),
        };
        let listed: Vec<MapJson> = poll
            .part(&format!(
                "/issues?labels={MAP_LABEL}&state=open&per_page={PER_PAGE}"
            ))
            .await?;
        let mut maps = Vec::new();
        for map in listed.into_iter().filter(|map| map.pull_request.is_none()) {
            let tickets: Vec<TicketJson> = poll
                .part(&format!(
                    "/issues/{}/sub_issues?per_page={PER_PAGE}",
                    map.number
                ))
                .await?;
            let mut issues = Vec::new();
            for ticket in tickets {
                let blocked = ticket
                    .issue_dependencies_summary
                    .as_ref()
                    .is_none_or(|summary| summary.total_blocked_by > 0);
                let blocked_by = if blocked {
                    poll.part::<Vec<BlockerJson>>(&format!(
                        "/issues/{}/dependencies/blocked_by?per_page={PER_PAGE}",
                        ticket.number
                    ))
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
            memo: poll.memo,
        })
    }
}

/// One poll's requests, with the memo it builds for the next.
struct MapsPoll<'a> {
    host: &'a GhGitHost,
    access: &'a HostAccountAccess,
    repository: &'a str,
    /// The last poll's memo.
    last: Option<&'a PollMemo>,
    memo: PollMemo,
}

impl MapsPoll<'_> {
    /// One part of the maps: fresh from the git host, or the last poll's
    /// answer when the git host answers that it has not changed.
    async fn part<T: DeserializeOwned + Serialize>(
        &mut self,
        path: &str,
    ) -> Result<T, GitHostError> {
        let answer_key = format!("{path}{ANSWER}");
        let known = self.last.and_then(|last| {
            Some((
                last.get(path)?.to_owned(),
                last.get(&answer_key)?.to_owned(),
            ))
        });
        let unexpected = |error: serde_json::Error| GitHostError::UnexpectedAnswer {
            answer: format!("{path}: {error}"),
        };
        match self
            .host
            .conditional(
                self.access,
                self.repository,
                path,
                known.as_ref().map(|(tag, _)| tag.as_str()),
            )
            .await?
        {
            Answer::NotModified => {
                let (tag, kept) = known.ok_or_else(|| GitHostError::UnexpectedAnswer {
                    answer: format!("304 for {path} without a tag"),
                })?;
                self.memo.insert(path, &tag);
                self.memo.insert(&answer_key, &kept);
                serde_json::from_str(&kept).map_err(unexpected)
            }
            Answer::Fresh { tag, body } => {
                let part: T = serde_json::from_str(&body).map_err(unexpected)?;
                if let Some(tag) = tag {
                    // Kept cut down to the fields above, not GitHub's whole
                    // answer with every issue's body.
                    let kept = serde_json::to_string(&part).map_err(unexpected)?;
                    self.memo.insert(path, &tag);
                    self.memo.insert(&answer_key, &kept);
                }
                Ok(part)
            }
        }
    }
}

impl TicketJson {
    fn read(self, blocked_by: Vec<Blocker>) -> TicketIssue {
        TicketIssue {
            kind: self.labels.iter().find_map(|label| kind(&label.name)),
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

/// The ticket type a label names, such as `wayfinder:research`.
fn kind(label: &str) -> Option<TicketKind> {
    match label.strip_prefix(KIND_LABEL)? {
        "research" => Some(TicketKind::Research),
        "prototype" => Some(TicketKind::Prototype),
        "grilling" => Some(TicketKind::Grilling),
        "task" => Some(TicketKind::Task),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{BlockerJson, IssueStateJson, kind};
    use crate::TicketKind;
    use crate::git_host::IssueRef;

    #[test]
    fn reads_the_ticket_type_from_its_label() {
        assert_eq!(kind("wayfinder:research"), Some(TicketKind::Research));
        assert_eq!(kind("wayfinder:task"), Some(TicketKind::Task));
        assert_eq!(kind("wayfinder:map"), None);
        assert_eq!(kind("research"), None);
    }

    #[test]
    fn tells_blockers_of_other_repositories_apart() {
        let blocker = |url: &str| BlockerJson {
            number: 5,
            state: IssueStateJson::Open,
            repository_url: url.to_owned(),
        };
        assert_eq!(
            blocker("https://api.github.com/repos/Owner/Repo")
                .read("owner/repo")
                .issue,
            IssueRef::Here { number: 5 }
        );
        assert_eq!(
            blocker("https://api.github.com/repos/other/lib")
                .read("owner/repo")
                .issue,
            IssueRef::Elsewhere {
                repository: "other/lib".to_owned(),
                number: 5
            }
        );
    }
}
