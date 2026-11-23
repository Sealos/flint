//! The connection state of clusters kept running together: what each one is doing
//! ([`member`]), when the next background attempt may start ([`schedule`]), when the selected
//! cluster tries again ([`retry`]), and whose credential may run ([`policy`]).
//!
//! The first three are pure: time and jitter are passed in, and nothing there sleeps, spawns or
//! does I/O. The caller owns the connections and carries out the actions these types return.

// Nothing calls this model until the cluster manager is wired to it.
#![allow(dead_code)]

pub(crate) mod member;
pub mod policy;
pub(crate) mod retry;
pub(crate) mod schedule;
