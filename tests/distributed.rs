use opcode_router::{DistributedDispatchError, DistributedRouter, declare_distributed_router, distributed_routes};

#[derive(Debug, PartialEq, Eq)]
enum TestError {
    Rejected,
}

struct Handler {
    value: u32,
}

declare_distributed_router! {
    static TEST_ROUTES;
    entry = TestRoute;
    target = Handler;
    id = u16;
    args = (u32,);
    error = TestError;
}

declare_distributed_router! {
    static DUPLICATE_ROUTES;
    entry = DuplicateRouteEntry;
    target = Handler;
    id = u16;
    args = (u32,);
    error = TestError;
}

#[distributed_routes(registry = DUPLICATE_ROUTES, entry = DuplicateRouteEntry)]
impl Handler {
    #[route(id = 9u16)]
    fn duplicate_first(&mut self, _amount: u32) {}
}

#[distributed_routes(registry = DUPLICATE_ROUTES, entry = DuplicateRouteEntry)]
impl Handler {
    #[route(id = 9u16)]
    fn duplicate_second(&mut self, _amount: u32) {}
}

#[distributed_routes(registry = TEST_ROUTES, entry = TestRoute)]
impl Handler {
    #[route(id = 1u16)]
    async fn add(&mut self, amount: u32) -> Result<(), TestError> {
        self.value += amount;
        Ok(())
    }

    #[route(id = 2u16)]
    fn reject(&mut self, _amount: u32) -> Result<(), TestError> {
        Err(TestError::Rejected)
    }
}

#[test]
fn dispatches_routes_collected_by_linkme() {
    let router = DistributedRouter::new(&TEST_ROUTES).unwrap();
    let mut handler = Handler { value: 0 };

    block_on(router.dispatch(&mut handler, 1u16, (5,))).unwrap();
    assert_eq!(handler.value, 5);

    assert!(matches!(
        block_on(router.dispatch(&mut handler, 2u16, (0,))),
        Err(DistributedDispatchError::Handler(TestError::Rejected))
    ));
    assert!(matches!(block_on(router.dispatch(&mut handler, 404u16, (0,))), Err(DistributedDispatchError::RouteNotFound(404))));
}

#[test]
fn rejects_duplicate_distributed_routes_at_startup() {
    let duplicate = match DistributedRouter::new(&DUPLICATE_ROUTES) {
        Ok(_) => panic!("duplicate routes must be rejected"),
        Err(duplicate) => duplicate,
    };

    assert_eq!(duplicate.id, 9u16);
    assert_ne!(duplicate.first, duplicate.duplicate);
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::{
        pin::pin,
        task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
    };

    fn raw_waker() -> RawWaker {
        fn clone(_: *const ()) -> RawWaker {
            raw_waker()
        }
        fn wake(_: *const ()) {}
        fn wake_by_ref(_: *const ()) {}
        fn drop(_: *const ()) {}

        RawWaker::new(std::ptr::null(), &RawWakerVTable::new(clone, wake, wake_by_ref, drop))
    }

    let waker = unsafe { Waker::from_raw(raw_waker()) };
    let mut context = Context::from_waker(&waker);
    let mut future = pin!(future);

    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}
