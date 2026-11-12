//! chainz-rpc against real sockets: the client, retries, probes and
//! capability checks over a mock upstream, and the control client against
//! servers that misbehave. One test binary; a module per area.

mod capabilities;
mod client;
mod common;
mod control;
mod probe;
mod upstream;
