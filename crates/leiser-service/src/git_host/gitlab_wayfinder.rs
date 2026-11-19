//! Polling a repository's wayfinder maps on GitLab with `glab api`: open
//! issues labelled `wayfinder:map`, each map's same-project linked ticket
//! issues, and the `is_blocked_by` issue links blocking each ticket. Every
//! request is conditional (`If-None-Match`), so unchanged answers return
//! `304 Not Modified` and are reused from the last poll's memo.

use serde::{Deserialize, Serialize};

use super::gitlab::GlabGitHost;
use super::gitlab_poll::{Requests, all_pages};
use super::http::{MAP_LABEL, ticket_type_of_label};
use super::{
    Blocker, GitHostError, HostAccountAccess, MapIssue, PolledWayfinderMaps, TicketIssue,
    encode_path_segment, https_project_path,
};
use crate::IssueRef;

#[derive(Deserialize, Serialize)]
struct MapJson {
    iid: u64,
    title: String,
    web_url: String,
}

#[derive(Deserialize, Serialize)]
struct LinkedIssueJson {
    iid: u64,
    title: String,
    web_url: String,
    state: IssueStateJson,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    assignees: Vec<AssigneeJson>,
    #[serde(default)]
    references: Option<ReferencesJson>,
    #[serde(default)]
    link_type: LinkTypeJson,
}

#[derive(Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum IssueStateJson {
    Opened,
    Closed,
}

#[derive(Deserialize, Serialize)]
struct AssigneeJson {
    username: String,
}

#[derive(Deserialize, Serialize)]
struct ReferencesJson {
    /// Such as `group/subgroup/project#14`.
    #[serde(default)]
    full: Option<String>,
}

#[derive(Deserialize, Serialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum LinkTypeJson {
    #[default]
    RelatesTo,
    Blocks,
    IsBlockedBy,
    #[serde(other)]
    Other,
}

impl GlabGitHost {
    /// Polls the open wayfinder maps of the GitLab project at `remote`.
    pub(super) async fn poll_maps(
        &self,
        access: &HostAccountAccess,
        remote: &str,
        last: Option<&PolledWayfinderMaps>,
    ) -> Result<PolledWayfinderMaps, GitHostError> {
        let project = https_project_path(remote, &access.git_host)?;
        let encoded_project = encode_path_segment(&project);
        let mut requests =
            Requests::new(self, access, &encoded_project, last.map(|last| &last.memo));
        let listed: Vec<MapJson> = all_pages(
            &mut requests,
            &format!("/issues?labels={MAP_LABEL}&state=opened&order_by=created_at&sort=desc"),
        )
        .await?;
        let mut maps = Vec::new();
        for map in listed {
            let linked: Vec<LinkedIssueJson> =
                all_pages(&mut requests, &format!("/issues/{}/links?", map.iid)).await?;
            let mut tickets = Vec::new();
            for ticket in linked.into_iter().filter(|issue| {
                issue.link_type != LinkTypeJson::IsBlockedBy && is_same_project(issue, &project)
            }) {
                let ticket_links: Vec<LinkedIssueJson> =
                    all_pages(&mut requests, &format!("/issues/{}/links?", ticket.iid)).await?;
                let blocked_by = ticket_links
                    .into_iter()
                    .filter(|link| link.link_type == LinkTypeJson::IsBlockedBy)
                    .map(|blocker| blocker.into_blocker(&project))
                    .collect();
                tickets.push(ticket.into_ticket(blocked_by));
            }
            maps.push(MapIssue {
                number: map.iid,
                title: map.title,
                url: map.web_url,
                tickets,
            });
        }
        Ok(PolledWayfinderMaps {
            maps,
            memo: requests.finish(),
        })
    }
}

fn issue_project_path(issue: &LinkedIssueJson) -> Option<&str> {
    issue
        .references
        .as_ref()?
        .full
        .as_deref()?
        .rsplit_once('#')
        .map(|(project, _)| project)
        .filter(|project| !project.is_empty())
}

fn is_same_project(issue: &LinkedIssueJson, project: &str) -> bool {
    issue_project_path(issue).is_none_or(|path| path.eq_ignore_ascii_case(project))
}

impl LinkedIssueJson {
    fn into_ticket(self, blocked_by: Vec<Blocker>) -> TicketIssue {
        TicketIssue {
            ticket_type: self
                .labels
                .iter()
                .find_map(|label| ticket_type_of_label(label)),
            number: self.iid,
            title: self.title,
            url: self.web_url,
            open: self.state == IssueStateJson::Opened,
            assignees: self
                .assignees
                .into_iter()
                .map(|user| user.username)
                .collect(),
            blocked_by,
        }
    }

    fn into_blocker(self, project: &str) -> Blocker {
        let issue = match issue_project_path(&self) {
            Some(path) if !path.eq_ignore_ascii_case(project) => IssueRef::Elsewhere {
                repository: path.to_owned(),
                number: self.iid,
            },
            _ => IssueRef::Here { number: self.iid },
        };
        Blocker {
            issue,
            open: self.state == IssueStateJson::Opened,
        }
    }
}
