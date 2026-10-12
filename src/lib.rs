//! Compile-time numeric opcode routing for synchronous and asynchronous methods.
//!
//! Currently this crate re-exports a small method-dispatch macro:
//!
//! ```rust
//! use opcode_router::route_methods;
//!
//! struct User {
//!     name: String,
//! }
//!
//! #[route_methods(id_type = u16)]
//! impl User {
//!     #[route(id = 7u16)]
//!     fn hello(&mut self) {
//!         self.name.push_str(" hello");
//!     }
//! }
//!
//! let mut user = User {
//!     name: "Tao".to_string(),
//! };
//!
//! assert!(user.call_by_id(7u16).is_ok());
//! assert_eq!(
//!     user.call_by_id(8u16),
//!     Err(opcode_router::CallError::IdNotFound("8".to_string()))
//! );
//! ```

pub use opcode_router_macros::{define_static_dispatch, define_static_router, distributed_routes, reflect_methods, reflect_methods_group, route_methods, route_methods_group};

extern crate self as opcode_router;
// Compatibility for existing users that imported the original package name.
extern crate self as asktao_tools;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    NameNotFound(String),
    IdNotFound(String),
}

pub type CallResult = Result<(), CallError>;

/// Converts a handler output into the router's common result type.
///
/// Routed methods may currently return either `()` or [`CallResult`]. This
/// keeps existing handlers source-compatible while ensuring errors are never
/// silently discarded.
pub trait IntoCallResult {
    fn into_call_result(self) -> CallResult;
}

impl IntoCallResult for () {
    fn into_call_result(self) -> CallResult {
        Ok(())
    }
}

impl IntoCallResult for CallResult {
    fn into_call_result(self) -> CallResult {
        self
    }
}

impl std::fmt::Display for CallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NameNotFound(name) => write!(formatter, "route name not found: {name}"),
            Self::IdNotFound(id) => write!(formatter, "route id not found: {id}"),
        }
    }
}

impl std::error::Error for CallError {}

/// Boxed future used by handlers registered across crate boundaries.
pub type RouteFuture<'a, E> = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), E>> + Send + 'a>>;

/// Uniform function pointer stored by a distributed route registry.
pub type RouteHandler<T, Args, E> = for<'a> fn(&'a mut T, Args) -> RouteFuture<'a, E>;

/// A statically linked route contributed by this crate or a dependent crate.
pub struct RouteEntry<T, Id, Args, E> {
    pub id: Id,
    pub name: &'static str,
    pub handler: RouteHandler<T, Args, E>,
}

impl<T, Id, Args, E> RouteEntry<T, Id, Args, E> {
    pub const fn new(id: Id, name: &'static str, handler: RouteHandler<T, Args, E>) -> Self {
        Self { id, name, handler }
    }
}

/// Normalizes distributed handler outputs without discarding errors.
pub trait IntoRouteResult<E> {
    fn into_route_result(self) -> Result<(), E>;
}

impl<E> IntoRouteResult<E> for () {
    fn into_route_result(self) -> Result<(), E> {
        Ok(())
    }
}

impl<E> IntoRouteResult<E> for Result<(), E> {
    fn into_route_result(self) -> Result<(), E> {
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateRoute<Id> {
    pub id: Id,
    pub first: &'static str,
    pub duplicate: &'static str,
}

impl<Id: std::fmt::Debug> std::fmt::Display for DuplicateRoute<Id> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "duplicate route id {:?}: `{}` and `{}`", self.id, self.first, self.duplicate)
    }
}

impl<Id: std::fmt::Debug> std::error::Error for DuplicateRoute<Id> {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DistributedDispatchError<Id, E> {
    RouteNotFound(Id),
    Handler(E),
}

impl<Id: std::fmt::Debug, E: std::fmt::Display> std::fmt::Display for DistributedDispatchError<Id, E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RouteNotFound(id) => write!(formatter, "route id not found: {id:?}"),
            Self::Handler(error) => write!(formatter, "route handler failed: {error}"),
        }
    }
}

impl<Id: std::fmt::Debug, E: std::error::Error + 'static> std::error::Error for DistributedDispatchError<Id, E> {}

/// Validated, immutable lookup table for routes collected by `linkme`.
pub struct DistributedRouter<T: 'static, Id: 'static, Args: 'static, E: 'static> {
    entries: &'static [RouteEntry<T, Id, Args, E>],
    index: std::collections::HashMap<Id, usize>,
}

impl<T: 'static, Id, Args: 'static, E: 'static> DistributedRouter<T, Id, Args, E>
where
    Id: Copy + Eq + std::hash::Hash + std::fmt::Debug + 'static,
{
    pub fn new(entries: &'static [RouteEntry<T, Id, Args, E>]) -> Result<Self, DuplicateRoute<Id>> {
        let mut index = std::collections::HashMap::with_capacity(entries.len());

        for (position, entry) in entries.iter().enumerate() {
            if let Some(first_position) = index.insert(entry.id, position) {
                return Err(DuplicateRoute {
                    id: entry.id,
                    first: entries[first_position].name,
                    duplicate: entry.name,
                });
            }
        }

        Ok(Self { entries, index })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub async fn dispatch(&self, target: &mut T, id: Id, args: Args) -> Result<(), DistributedDispatchError<Id, E>> {
        let Some(position) = self.index.get(&id).copied() else {
            return Err(DistributedDispatchError::RouteNotFound(id));
        };

        let entry = &self.entries[position];
        (entry.handler)(target, args).await.map_err(DistributedDispatchError::Handler)
    }
}

/// Declares a linker-collected route registry that dependent crates can extend.
#[macro_export]
macro_rules! declare_distributed_router {
    (
        $(#[$meta:meta])*
        $visibility:vis static $registry:ident;
        entry = $entry:ident;
        target = $target:ty;
        id = $id:ty;
        args = $args:ty;
        error = $error:ty $(;)?
    ) => {
        $visibility type $entry = $crate::RouteEntry<$target, $id, $args, $error>;

        $(#[$meta])*
        #[$crate::__private::distributed_slice]
        #[linkme(crate = $crate::__private::linkme)]
        $visibility static $registry: [$entry];
    };
}

#[doc(hidden)]
pub mod __private {
    pub use linkme;
    pub use linkme::distributed_slice;
}

#[cfg(test)]
mod tests {
    use crate::{define_static_dispatch, reflect_methods};

    #[test]
    fn calls_marked_method_by_name() {
        use crate::reflect_methods;

        struct User {
            value: i32,
        }

        #[reflect_methods]
        impl User {
            #[callable(id = 1)]
            fn hello(&mut self) {
                self.value += 1;
            }

            #[reflect_ignore]
            #[allow(dead_code)]
            fn internal(&mut self) {
                self.value += 100;
            }
        }

        let mut user = User { value: 0 };

        assert!(user.call_by_name("hello").is_ok());
        assert_eq!(user.value, 1);
        assert_eq!(user.call_by_name("internal"), Err(crate::CallError::NameNotFound("internal".to_string())));
    }

    #[test]
    fn forwards_method_parameters() {
        use crate::reflect_methods;

        struct Actor {
            value: i32,
        }

        struct User;

        #[reflect_methods]
        impl User {
            #[callable(id = 1001)]
            fn say_hello(&mut self, actor: &mut Actor, data: String) {
                actor.value += data.len() as i32;
            }

            #[callable(id = 1002)]
            fn say_world(&mut self, actor: &mut Actor, data: String) {
                actor.value += data.len() as i32 * 2;
            }
        }

        let mut user = User;
        let mut actor = Actor { value: 0 };

        assert!(user.call_by_name("say_hello", &mut actor, "abc".to_string()).is_ok());
        assert_eq!(actor.value, 3);

        assert!(user.call_by_id(1001, &mut actor, "abcd".to_string()).is_ok());
        assert_eq!(actor.value, 7);

        assert!(user.call_by_name("say_world", &mut actor, "ab".to_string()).is_ok());
        assert_eq!(actor.value, 11);

        assert_eq!(user.call_by_id(404, &mut actor, "ab".to_string()), Err(crate::CallError::IdNotFound("404".to_string())));
    }

    #[test]
    fn supports_mixed_sync_and_async_methods() {
        use crate::reflect_methods;

        struct Actor {
            value: i32,
        }

        struct User;

        #[reflect_methods]
        impl User {
            #[callable(id = 2000)]
            fn say_sync(&mut self, actor: &mut Actor, data: i32) {
                actor.value += data;
            }

            #[callable(id = 2001)]
            async fn say_async(&mut self, actor: &mut Actor, data: i32) {
                actor.value += data * 2;
            }
        }

        let mut user = User;
        let mut actor = Actor { value: 0 };

        block_on(user.call_by_name("say_sync", &mut actor, 3)).unwrap();
        assert_eq!(actor.value, 3);

        block_on(user.call_by_id(2001, &mut actor, 4)).unwrap();
        assert_eq!(actor.value, 11);
    }

    #[test]
    fn propagates_handler_errors() {
        use crate::route_methods;

        struct Handler;

        #[route_methods(id_type = u16)]
        impl Handler {
            #[route(id = 9u16)]
            async fn fail(&mut self) -> crate::CallResult {
                Err(crate::CallError::NameNotFound("handler failed".to_string()))
            }
        }

        let mut handler = Handler;
        assert_eq!(block_on(handler.call_by_id(9u16)), Err(crate::CallError::NameNotFound("handler failed".to_string())));
    }

    #[test]
    fn supports_u16_id_type_option() {
        use crate::reflect_methods;

        struct User {
            value: i32,
        }

        #[reflect_methods(id_type = u16)]
        impl User {
            #[callable(id = 7u16)]
            fn lucky(&mut self) {
                self.value += 7;
            }
        }

        let mut user = User { value: 0 };

        user.call_by_id(7u16).unwrap();
        assert_eq!(user.value, 7);

        assert_eq!(user.call_by_id(8u16), Err(crate::CallError::IdNotFound("8".to_string())));
    }

    #[test]
    fn supports_multiple_impl_blocks_with_custom_dispatch_names() {
        use crate::reflect_methods;

        struct User {
            value: i32,
        }

        #[reflect_methods(name_fn = call_first_by_name, id_fn = call_first_by_id)]
        impl User {
            #[callable(id = 1)]
            fn first(&mut self) {
                self.value += 1;
            }
        }

        #[reflect_methods(name_fn = call_second_by_name, id_fn = call_second_by_id)]
        impl User {
            #[callable(id = 2)]
            fn second(&mut self) {
                self.value += 2;
            }
        }

        let mut user = User { value: 0 };

        user.call_first_by_name("first").unwrap();
        user.call_second_by_id(2).unwrap();

        assert_eq!(user.value, 3);
        assert_eq!(user.call_first_by_id(404), Err(crate::CallError::IdNotFound("404".to_string())));
    }

    #[test]
    fn groups_multiple_impl_blocks_into_one_dispatch_method() {
        use crate::reflect_methods_group;

        struct User {
            value: i32,
        }

        reflect_methods_group! {
            (id_type = u16)

            impl User {
                #[callable(id = 1u16)]
                fn first(&mut self) {
                    self.value += 1;
                }
            }

            impl User {
                #[callable(id = 2u16)]
                fn second(&mut self) {
                    self.value += 2;
                }
            }
        }

        let mut user = User { value: 0 };

        user.call_by_id(1u16).unwrap();
        user.call_by_name("second").unwrap();

        assert_eq!(user.value, 3);
        assert_eq!(user.call_by_id(404u16), Err(crate::CallError::IdNotFound("404".to_string())));
    }

    #[test]
    fn defines_static_dispatch_by_scanning_source() {
        let mut user = StaticDispatchUser { value: 0 };

        user.call_by_id(301u16, 3).unwrap();
        user.call_by_name("scanned_second", 4).unwrap();

        assert_eq!(user.value, 11);
    }

    struct StaticDispatchUser {
        value: i32,
    }

    #[reflect_methods(collect_only)]
    impl StaticDispatchUser {
        #[callable(id = 301u16)]
        fn scanned_first(&mut self, data: i32) {
            self.value += data;
        }
    }

    #[reflect_methods(collect_only)]
    impl StaticDispatchUser {
        #[callable(id = 302u16)]
        fn scanned_second(&mut self, data: i32) {
            self.value += data * 2;
        }
    }

    define_static_dispatch! {
        target = StaticDispatchUser,
        id_type = u16,
        scan = "src/lib.rs",
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
        let mut cx = Context::from_waker(&waker);
        let mut future = pin!(future);

        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
                return output;
            }
        }
    }
}
