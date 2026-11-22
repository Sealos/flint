//! The connection state of clusters kept running together: what each one is doing
//! ([`member`]) and when the next background attempt may start ([`schedule`]).
//!
//! Both are pure: time and jitter are passed in, and nothing here sleeps, spawns or does I/O.
//! The caller owns the connections and carries out the actions these types return.

// Nothing calls this model until the cluster manager is wired to it.
#![allow(dead_code)]

pub(crate) mod member;
pub(crate) mod schedule;
