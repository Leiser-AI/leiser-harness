//! Test kit for the harness service.
//!
//! Tests start the harness service in-process with a [`FakeDriver`], which
//! runs the scripted `fake-agent-cli` program instead of a real agent CLI,
//! and a temporary git repository ([`TestRepo`]). No subscription and no
//! network are needed.

mod fake_driver;
mod repo;
mod script;
mod turn;

pub use fake_driver::FakeDriver;
pub use repo::TestRepo;
pub use script::{FakeInput, Script, Step};
pub use turn::{TurnError, collect_turn, collect_turn_within};
