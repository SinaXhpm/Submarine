//! The agent gateway: AI agents on this computer work on the user's servers
//! through Submarine, with the user's approval for each access, and never get
//! a password, a key or an address. (Not the SSH agent: that is
//! `agent_auth`.)
//!
//! - `mcp`: the protocol agents speak, one JSON message a line.
//! - `tools`: what an agent can call, and the checks on what it sends.
//! - `grant`: what the user can give, and when it ends.
//! - `serve`: one agent connection, from its first line to its last.
//! - `ipc`: the pipe or socket agents connect to, and who is on its other end.
//! - `bridge`: `submarine mcp`, the program an agent starts.
//! - `state`: who is connected, what they asked for, what they were given.
//! - `mask`: hiding the address Submarine connects to in what an agent reads.
//! - `ops`: one operation of a grant, from the call to the answer.
//! - `hub`: the running side: the listener, the state, the page's commands.

pub mod bridge;
pub mod grant;
pub mod hub;
pub mod ipc;
pub mod mask;
pub mod mcp;
pub mod ops;
pub mod serve;
pub mod state;
pub mod tools;
