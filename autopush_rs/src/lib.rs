//! WIP: Implementation of WebPush ("autopush" as well) in Rust
//!
//! This crate currently provides an implementation of an asynchronous WebPush
//! server which is intended to be interfaced with from Python. The crate mostly
//! has a C API which is driven from `__init__.py` in Python and orchestrated
//! from Python. This is currently done to help ease the transition from the old
//! Python implementation to the new Rust implementation. Currently there's a
//! good bit of API calls to remote services still implemented in Python, but
//! the thinking is that over time these services will be rewritten in to Rust
//! and the Python codebase will shrink.
//!
//! In any case though, this'll focus mainly on the Rust bits rather than the
//! Python bits! It's worth nothing though that this crate is intended to be
//! used with `cffi` in Python, which is "seamlessly" worked with through the
//! `snaek` Python dependency. That basically just means that Python "headers"
//! for this Rust crate are generated automatically.
//!
//! # High level overview
//!
//! At 10,000 feet the general architecture here is that the main Python thread
//! spins up a Rust thread which actually does all the relevant I/O. The one
//! Rust thread uses a `Core` from `tokio-core` to perform all I/O and schedule
//! asynchronous tasks. The `tungstenite` crate is used to parse and manage the
//! WebSocket protocol, with `tokio_tungstenite` being a nicer wrapper for
//! futures-style APIs.
//!
//! The entire server is written in an asynchronous fashion using the `futures`
//! crate in Rust. This basically just means that everything is exposed as a
//! future (similar to the concept in other languages) and that's how bits and
//! pieces are chained together.
//!
//! Each connected client maintains a state machine of where it is in the
//! webpush protocol (see `states.dot` at the root of this repository). Note
//! that not all states are implemented yet, this is a work in progress! All I/O
//! is managed by Rust and various state transitions are managed by Rust as
//! well. Movement between states happens typically as a result of calls into
//! Python. The various operations here will call into Python to do things like
//! db/HTTP requests and then the results are interpreted in Rust to progress
//! the state machine.
//!
//! # Module index
//!
//! There's a number of modules that currently make up the Rust implementation,
//! and one-line summaries of these are:
//!
//! * `queue` - a MPMC queue which is used to send messages to Python and Python
//!   uses to delegate work to worker threads.
//! * `server` - the main bindings for the WebPush server, where the tokio
//!   `Core` is created and managed inside of the Rust thread.
//! * `client` - this is where the state machine for each connected client is
//!   located, managing connections over time and sending out notifications as
//!   they arrive.
//! * `protocol` - a definition of the WebPush protocol messages which are send
//!   over websockets.
//! * `call` - definitions of various calls that can be made into Python, each
//!   of which returning a future of the response.
//!
//! Other modules tend to be miscellaneous implementation details and likely
//! aren't as relevant to the WebPush implementation.
//!
//! Otherwise be sure to check out each module for more documentation!

extern crate bytes;
extern crate cadence;
extern crate chrono;
#[macro_use]
extern crate futures;
extern crate httparse;
extern crate hyper;
extern crate libc;
extern crate openssl;
extern crate sentry;
extern crate serde;
#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate slog;
#[macro_use]
extern crate slog_scope;
extern crate slog_term;
extern crate slog_async;
extern crate slog_json;
extern crate slog_stdlog;
#[macro_use]
extern crate serde_json;
extern crate time;
extern crate tokio_core;
extern crate tokio_io;
extern crate tokio_openssl;
extern crate tokio_service;
extern crate tokio_tungstenite;
extern crate tungstenite;
extern crate uuid;
extern crate woothee;

#[macro_use]
extern crate error_chain;

mod client;
mod errors;
mod http;
mod protocol;
mod util;

#[macro_use]
pub mod rt;
pub mod call;
pub mod server;
pub mod queue;
