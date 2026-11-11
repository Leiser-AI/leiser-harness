//! Wayfinder maps: while a client such as the desktop app is connected,
//! the harness service polls every repository's open wayfinder maps on the
//! git host every [`POLL_INTERVAL`], with the repository's host account and
//! the entity tags of the last answers, the same way auto-fix polls a pull
//! request. A change is reported as [`Event::WayfinderMapsChanged`]; the
//! maps themselves are read with
//! [`Command::ReadWayfinderMaps`](crate::Command::ReadWayfinderMaps).

use std::collections::BTreeSet;
use std::time::Duration;

use super::auto_fix::PollError;
use super::{Background, State};
use crate::git_host::{IssueRef, MapIssue, PolledWayfinderMaps, TicketIssue};
use crate::{
    CommandError, Event, HostAccountBinding, RepositoryId, Response, Ticket, TicketState,
    WayfinderMap, WayfinderMaps,
};

/// How often a repository's wayfinder maps are polled.
pub(super) const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// A repository's wayfinder maps as the harness service keeps them. Not
/// stored: the first poll after a restart gets full answers.
pub(super) struct RepositoryMaps {
    shown: WayfinderMaps,
    /// The last successful poll's answer, whose memo the next poll gets.
    last: Option<PolledWayfinderMaps>,
    polling: bool,
}

impl Default for RepositoryMaps {
    fn default() -> Self {
        Self {
            shown: WayfinderMaps::NotRead,
            last: None,
            polling: false,
        }
    }
}

/// A poll of a repository's wayfinder maps, done in the background.
pub(super) struct WayfinderWork {
    repository: RepositoryId,
    result: Result<PolledWayfinderMaps, PollError>,
}

impl State {
    pub(super) fn read_wayfinder_maps(
        &mut self,
        repository: RepositoryId,
    ) -> Result<Response, CommandError> {
        if !self.repositories.contains_key(&repository) {
            return Err(CommandError::UnknownRepository { repository });
        }
        let maps = self.wayfinder.entry(repository).or_default();
        if maps.shown == WayfinderMaps::NotRead && !maps.polling {
            self.poll_wayfinder(repository);
        }
        let maps = self
            .wayfinder
            .get(&repository)
            .map_or(WayfinderMaps::NotRead, |maps| maps.shown.clone());
        Ok(Response::WayfinderMaps { maps })
    }

    /// Polls every repository's maps that is not being polled, while a
    /// client listens for the result.
    pub(super) fn poll_wayfinder_maps(&mut self) {
        // A poll would keep the update waiting; nobody would see it.
        if self.update_waiting || self.events.receiver_count() == 0 {
            return;
        }
        let repositories: Vec<RepositoryId> = self.repositories.keys().copied().collect();
        for repository in repositories {
            if !self
                .wayfinder
                .get(&repository)
                .is_some_and(|maps| maps.polling)
            {
                self.poll_wayfinder(repository);
            }
        }
    }

    fn poll_wayfinder(&mut self, repository: RepositoryId) {
        let Some(path) = self.repositories.get(&repository).cloned() else {
            return;
        };
        let access = match self.host_account_binding(&path) {
            HostAccountBinding::Bound { host_account, .. } => {
                self.host_accounts.access(host_account)
            }
            _ => None,
        };
        let Some(access) = access else {
            self.show_wayfinder(
                repository,
                WayfinderMaps::Unavailable {
                    reason: PollError::NoHostAccount.to_string(),
                },
            );
            return;
        };
        let maps = self.wayfinder.entry(repository).or_default();
        maps.polling = true;
        let last = maps.last.clone();
        let background = self.background.clone();
        let git_host = self.git_host.clone();
        self.background_running += 1;
        tokio::spawn(async move {
            let result = async {
                let remote = crate::git::origin_url(&path)
                    .await
                    .ok_or(PollError::NoOrigin)?;
                Ok(git_host
                    .poll_wayfinder_maps(&access, &remote, last.as_ref())
                    .await?)
            }
            .await;
            // The service has stopped; the next start polls again.
            let _ = background
                .send(Background::Wayfinder(WayfinderWork { repository, result }))
                .await;
        });
    }

    pub(super) fn wayfinder_polled(&mut self, work: WayfinderWork) {
        let WayfinderWork { repository, result } = work;
        let Some(maps) = self.wayfinder.get_mut(&repository) else {
            return;
        };
        maps.polling = false;
        let shown = match result {
            Ok(polled) => {
                let shown = WayfinderMaps::Read {
                    maps: polled.maps.iter().map(map).collect(),
                };
                maps.last = Some(polled);
                shown
            }
            Err(error) => {
                tracing::warn!(?repository, %error, "could not poll the wayfinder maps");
                // The maps last read stay; the next poll tries again.
                if matches!(maps.shown, WayfinderMaps::Read { .. }) {
                    return;
                }
                WayfinderMaps::Unavailable {
                    reason: error.to_string(),
                }
            }
        };
        self.show_wayfinder(repository, shown);
    }

    /// Keeps `shown` as the repository's maps, and reports a change.
    fn show_wayfinder(&mut self, repository: RepositoryId, shown: WayfinderMaps) {
        let maps = self.wayfinder.entry(repository).or_default();
        if maps.shown != shown {
            maps.shown = shown;
            self.emit(Event::WayfinderMapsChanged { repository });
        }
    }
}

/// A map as the developer sees it: its tickets' states, and the edges
/// between its own tickets.
fn map(issue: &MapIssue) -> WayfinderMap {
    let in_map: BTreeSet<u64> = issue.tickets.iter().map(|ticket| ticket.number).collect();
    WayfinderMap {
        number: issue.number,
        title: issue.title.clone(),
        url: issue.url.clone(),
        tickets: issue
            .tickets
            .iter()
            .map(|ticket| Ticket {
                number: ticket.number,
                title: ticket.title.clone(),
                url: ticket.url.clone(),
                kind: ticket.kind,
                state: state(ticket),
                blocked_by: ticket
                    .blocked_by
                    .iter()
                    .filter_map(|blocker| match blocker.issue {
                        IssueRef::Here { number } if in_map.contains(&number) => Some(number),
                        _ => None,
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn state(ticket: &TicketIssue) -> TicketState {
    let open_blockers: Vec<String> = ticket
        .blocked_by
        .iter()
        .filter(|blocker| blocker.open)
        .map(|blocker| blocker.issue.to_string())
        .collect();
    if !ticket.open {
        TicketState::Closed
    } else if !ticket.assignees.is_empty() {
        TicketState::Claimed {
            assignees: ticket.assignees.clone(),
        }
    } else if !open_blockers.is_empty() {
        TicketState::Blocked { open_blockers }
    } else {
        TicketState::Frontier
    }
}
