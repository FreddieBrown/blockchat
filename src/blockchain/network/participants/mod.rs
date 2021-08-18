//! Functionality for different network roles

mod miners;
mod shared;
mod users;

pub use self::{miners::miners_state_machine, shared::runner, users::users_state_machine};
