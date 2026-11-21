//! The write timeout, on a paused clock: a server that never answers costs no real time.

use std::convert::Infallible;
use std::future::Future;

use http::{Request, Response};
use kube::client::Body;
use tokio::time::Instant;

use super::super::*;
use super::{EDIT, kind, target};

/// The promised limit, written out: comparing with [`WRITE_TIMEOUT`] would pass at any value.
const PROMISED: Duration = Duration::from_secs(30);

/// A client whose server accepts every request and never answers.
#[allow(clippy::disallowed_methods)] // No cluster and no credentials: a stub transport.
fn silent_client() -> Client {
    let service = tower::service_fn(|_: Request<Body>| {
        std::future::pending::<Result<Response<Body>, Infallible>>()
    });
    Client::new(service, "default")
}

/// Runs `request` against the silent server. It must fail at exactly [`PROMISED`], saying
/// the change may have applied: the outer limit fails the test if the timeout is gone or longer.
async fn assert_times_out<T: std::fmt::Debug>(
    request: impl Future<Output = Result<T, WriteError>>,
) {
    let started = Instant::now();
    let result = tokio::time::timeout(PROMISED * 2, request)
        .await
        .expect("the request is bounded by the write timeout");
    assert_eq!(started.elapsed(), PROMISED);
    assert_eq!(result.unwrap_err(), timed_out());
    assert!(
        timed_out()
            .to_string()
            .contains("may or may not have applied")
    );
}

#[tokio::test(start_paused = true)]
async fn a_put_the_server_never_answers_times_out() {
    let body = edit_body(EDIT, &kind(), &target()).unwrap();
    assert_times_out(replace(&silent_client(), &kind(), &target(), &body, false)).await;
}

#[tokio::test(start_paused = true)]
async fn a_dry_run_the_server_never_answers_times_out() {
    let body = edit_body(EDIT, &kind(), &target()).unwrap();
    assert_times_out(replace(&silent_client(), &kind(), &target(), &body, true)).await;
}

#[tokio::test(start_paused = true)]
async fn a_read_the_server_never_answers_times_out() {
    assert_times_out(read_live(&silent_client(), &kind(), &target())).await;
}
