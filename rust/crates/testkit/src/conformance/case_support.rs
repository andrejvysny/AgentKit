//! Shared plumbing of the memory-context suites: a case runner that spawns
//! each case on a fresh implementation and names the case on failure, and
//! the `suite!` macro that builds a suite's `CASES` list and `run`.

use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub type CaseFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
pub type Case<T> = (&'static str, fn(Arc<T>) -> CaseFuture);

/// Runs every case against an implementation built fresh by `make`.
/// Panics, naming the suite and case, on the first failure.
pub async fn run_cases<T, F, Fut>(suite: &str, cases: Vec<Case<T>>, make: F)
where
    T: Send + Sync + 'static,
    F: Fn() -> Fut,
    Fut: Future<Output = T>,
{
    for (name, case) in cases {
        let subject = Arc::new(make().await);
        if let Err(error) = tokio::spawn(case(subject)).await {
            let payload: Box<dyn Any + Send> = if error.is_panic() {
                error.into_panic()
            } else {
                Box::new(format!("case task cancelled: {error}"))
            };
            panic!(
                "{suite} conformance case `{name}` failed: {}",
                describe(payload)
            );
        }
    }
}

fn describe(payload: Box<dyn Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(text) => *text,
        Err(payload) => match payload.downcast::<&'static str>() {
            Ok(text) => (*text).to_owned(),
            Err(_) => "non-string panic".to_owned(),
        },
    }
}

/// Unwrap a port result or name the fault.
pub fn port<T, E: std::fmt::Debug>(outcome: Result<T, E>) -> T {
    match outcome {
        Ok(value) => value,
        Err(error) => panic!("port call failed: {error:?}"),
    }
}

/// `suite!("label", Bound, case_a, case_b, ..)` defines `CASES` and
/// `run(make)` for implementations of `Bound`; each case is
/// `async fn name<T: Bound>(subject: Arc<T>)`.
macro_rules! suite {
    ($label:literal, $bound:path, $($name:ident),+ $(,)?) => {
        /// Names of every case, in run order.
        pub const CASES: &[&str] = &[$(stringify!($name)),+];

        /// Runs every case against an implementation built fresh by `make`.
        pub async fn run<T, F, Fut>(make: F)
        where
            T: $bound + 'static,
            F: Fn() -> Fut,
            Fut: std::future::Future<Output = T>,
        {
            let cases: Vec<$crate::conformance::case_support::Case<T>> = vec![
                $((
                    stringify!($name),
                    (|subject| Box::pin($name(subject)))
                        as fn(::std::sync::Arc<T>) -> $crate::conformance::case_support::CaseFuture,
                )),+
            ];
            $crate::conformance::case_support::run_cases($label, cases, make).await;
        }
    };
}
