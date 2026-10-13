//! orchid-watch: an independent watchdog for the orchid-room thermostat.
//!
//! It polls orchidd's HTTP API from another machine and sends a text (via ntfy
//! and the iMessage relay) and optionally an email when the room needs a
//! person. It shares no code with orchidd on purpose.

pub mod config;
pub mod io;
pub mod model;
pub mod watch;
