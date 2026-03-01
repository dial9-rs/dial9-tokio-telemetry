use criterion::{Criterion, black_box, criterion_group, criterion_main};
use dial9_tokio_telemetry::task_dump::DetectLongWait;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use tokio::runtime::dump::Trace;
use tokio::sync::mpsc;

struct AlwaysPending;

impl Future for AlwaysPending {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Pending
    }
}

fn noop_waker() -> Waker {
    use std::task::{RawWaker, RawWakerVTable};

    unsafe fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(std::ptr::null(), &VTABLE)
    }
    unsafe fn wake(_: *const ()) {}
    unsafe fn wake_by_ref(_: *const ()) {}
    unsafe fn drop(_: *const ()) {}

    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);

    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}

fn bench_poll_overhead(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    c.bench_function("trace_capture", |b| {
        b.iter(|| {
            rt.block_on(async {
                let mut future = AlwaysPending;
                let waker = noop_waker();
                let mut cx = Context::from_waker(&waker);
                let (_result, trace) = Trace::capture(|| Pin::new(&mut future).poll(&mut cx));
                black_box(trace);
            });
        });
    });

    c.bench_function("first_poll_with_capture", |b| {
        b.iter(|| {
            rt.block_on(async {
                let (tx, _rx) = mpsc::unbounded_channel();
                let future = AlwaysPending;
                let mut wrapped = DetectLongWait::new(future, tx);
                let waker = noop_waker();
                let mut cx = Context::from_waker(&waker);
                let _ = black_box(Pin::new(&mut wrapped).poll(&mut cx));
            });
        });
    });
}

criterion_group!(benches, bench_poll_overhead);
criterion_main!(benches);
