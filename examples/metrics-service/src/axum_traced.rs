/// Minimal re-implementation of `axum::serve` that wraps both the accept-loop
/// future and every per-connection future in `Traced<F>` so that scheduling
/// delays are captured by the telemetry system.
use std::{convert::Infallible, fmt::Debug, future::Future, io, marker::PhantomData, pin::pin};

use axum::serve::Listener;
use axum_core::{body::Body, extract::Request, response::Response};
use dial9_tokio_telemetry::telemetry::TelemetryHandle;
use futures_util::FutureExt as _;
use hyper::body::Incoming;
use hyper_util::{rt::TokioIo, server::conn::auto::Builder, service::TowerToHyperService};
use tokio::sync::watch;
use tower::ServiceExt as _;
use tower_service::Service;

/// A hyper executor that routes spawns through dial9's TelemetryHandle
/// so HTTP/2 internal tasks get wake event tracking.
#[derive(Clone)]
struct TracedExecutor;

impl<Fut> hyper::rt::Executor<Fut> for TracedExecutor
where
    Fut: Future + Send + 'static,
    Fut::Output: Send + 'static,
{
    fn execute(&self, fut: Fut) {
        spawn(fut);
    }
}

/// Our own `IncomingStream`, mirroring `axum::serve::IncomingStream`.
/// We need this because axum's version has private fields and can only be
/// constructed inside the axum crate.
pub struct IncomingStream<'a, L: Listener> {
    #[allow(dead_code)]
    io: &'a TokioIo<L::Io>,
    #[allow(dead_code)]
    remote_addr: L::Addr,
}

pub fn serve<L, M, S>(listener: L, make_service: M) -> Serve<L, M, S>
where
    L: Listener,
    M: for<'a> Service<IncomingStream<'a, L>, Error = Infallible, Response = S>,
    S: Service<Request, Response = Response, Error = Infallible> + Clone + Send + 'static,
    S::Future: Send,
{
    Serve {
        listener,
        make_service,
        _marker: PhantomData,
    }
}

pub struct Serve<L, M, S> {
    listener: L,
    make_service: M,
    _marker: PhantomData<fn() -> S>,
}

impl<L, M, S> Serve<L, M, S>
where
    L: Listener,
{
    pub fn with_graceful_shutdown<F>(self, signal: F) -> WithGracefulShutdown<L, M, S, F>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        WithGracefulShutdown {
            listener: self.listener,
            make_service: self.make_service,
            signal,
            _marker: PhantomData,
        }
    }
}

pub struct WithGracefulShutdown<L, M, S, F> {
    listener: L,
    make_service: M,
    signal: F,
    _marker: PhantomData<fn() -> S>,
}

impl<L, M, S, F> std::future::IntoFuture for WithGracefulShutdown<L, M, S, F>
where
    L: Listener,
    L::Addr: Debug,
    M: for<'a> Service<IncomingStream<'a, L>, Error = Infallible, Response = S> + Send + 'static,
    for<'a> <M as Service<IncomingStream<'a, L>>>::Future: Send,
    S: Service<Request, Response = Response, Error = Infallible> + Clone + Send + 'static,
    S::Future: Send,
    F: Future<Output = ()> + Send + 'static,
{
    type Output = io::Result<()>;
    type IntoFuture = futures_util::future::BoxFuture<'static, io::Result<()>>;

    fn into_future(self) -> Self::IntoFuture {
        let Self {
            mut listener,
            mut make_service,
            signal,
            _marker,
        } = self;

        Box::pin(async move {
            let (signal_tx, signal_rx) = watch::channel(());
            spawn(async move {
                signal.await;
                drop(signal_rx);
            });

            let (close_tx, close_rx) = watch::channel(());

            loop {
                let (io, remote_addr) = tokio::select! {
                    conn = listener.accept() => conn,
                    _ = signal_tx.closed() => break,
                };

                let io = TokioIo::new(io);

                make_service.ready().await.unwrap_or_else(|e| match e {});
                let tower_service = make_service
                    .call(IncomingStream {
                        io: &io,
                        remote_addr,
                    })
                    .await
                    .unwrap_or_else(|e| match e {})
                    .map_request(|req: Request<Incoming>| req.map(Body::new));

                let hyper_service = TowerToHyperService::new(tower_service);
                let signal_tx = signal_tx.clone();
                let close_rx = close_rx.clone();

                spawn(async move {
                    let builder = Builder::new(TracedExecutor);
                    let conn = builder.serve_connection_with_upgrades(io, hyper_service);
                    let mut conn = pin!(conn);
                    let mut signal_closed = pin!(signal_tx.closed().fuse());

                    loop {
                        tokio::select! {
                            result = conn.as_mut() => {
                                if let Err(_err) = result {
                                    tracing::trace!("failed to serve connection: {_err:#}");
                                }
                                break;
                            }
                            _ = &mut signal_closed => {
                                conn.as_mut().graceful_shutdown();
                            }
                        }
                    }
                    drop(close_rx);
                });
            }

            drop(close_rx);
            drop(listener);
            close_tx.closed().await;
            Ok(())
        })
    }
}

fn spawn<F>(fut: F)
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    // `TelemetryHandle::current()` always returns a usable handle —
    // when telemetry is disabled, `spawn` falls through to
    // `tokio::spawn` without wake tracking.
    TelemetryHandle::current().spawn(fut);
}
