#![allow(incomplete_features)]
#![ cfg_attr( feature = "unstable-async-drop", feature( async_drop ) ) ]

use std::marker::{PhantomData, PhantomPinned};
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::{Acquire, Release};
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

#[cfg(feature="unstable-async-drop")]
mod unstable {
    use crate::{AsyncClosure, ScopedFuture};
    use std::future::AsyncDrop;
    use std::pin::Pin;

    impl<Fut: Future, C: AsyncClosure<Fut>> AsyncDrop for ScopedFuture<Fut, C> {
        async fn drop(self: Pin<&mut Self>) {
            // SAFETY: We are in drop - have exclusive access to Self, because self is Pinned and !Unpin (have PhantomPinned) we can assume that if Pin<&mut Self> then Pin<&mut self.completeness_holder>
            // Rest of the module makes sure that access to the &mut CompletenessHolder is same as &CompletenessHolder
            let pin = unsafe { Pin::new_unchecked(&mut self.get_unchecked_mut().completeness_holder)};
            pin.await;
        }
    }
}

pub struct CompletenessHolder {
    ref_count: AtomicUsize,
    waker: Mutex<Option<Waker>>,
    _phantom_pinned: PhantomPinned // We will share CompletenessHolder as &ref that's why it's better to make sure that is never moved after constructing Pin to it
}

impl Default for CompletenessHolder {
    fn default() -> Self {
        Self {
            ref_count: Default::default(),
            waker: Mutex::new(None),
            _phantom_pinned: Default::default(),
        }
    }
}

impl CompletenessHolder {
    const MAX_REFCOUNT: usize = isize::MAX as usize;

    fn inc(&self) {
        let old_size = self.ref_count.fetch_add(1, Release);
        if old_size > Self::MAX_REFCOUNT {
            std::process::abort();
        }
    }

    fn dec(&self) {
        self.ref_count.fetch_sub(1, Release);
    }

    fn is_completed(&self) -> bool {
        self.ref_count.load(Acquire) == 0
    }

    fn make_guard<'a>(&'a self) -> CompletenessGuard<'a> {
        self.inc();
        CompletenessGuard(self)
    }
}

impl Future for CompletenessHolder {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.is_completed() {
            Poll::Ready(())
        } else {
            self.waker.lock().unwrap().replace(cx.waker().clone());
            Poll::Pending
        }
    }
}


struct CompletenessGuard<'a>(&'a CompletenessHolder);

impl Drop for CompletenessGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        self.0.dec();

        let mut guard = self.0.waker.lock().unwrap();
        if self.0.is_completed() {
            match guard.take() {
                None => {}
                Some(waker) => waker.wake(),
            }
        }
    }
}

pub trait AsyncClosure<Fut: Future> {
    type OutputFuture: Future;
    type Output;

    fn call(self, arg: &'static CompletenessHolder) -> Self::OutputFuture;
}

impl<Fut: Future, F: FnOnce(&'static CompletenessHolder) -> Fut> AsyncClosure<Fut> for F {
    type OutputFuture = Fut;
    type Output = Fut::Output;

    fn call(self, arg: &'static CompletenessHolder) -> Self::OutputFuture {
        self(arg)
    }
}

enum FutureState<Fut: Future, C: AsyncClosure<Fut>> {
    NotEntered(Option<C>),
    Pending(<C as AsyncClosure<Fut>>::OutputFuture),
    Completed(Option<<<C as AsyncClosure<Fut>>::OutputFuture as Future>::Output>),
}

pub struct ScopedFuture<Fut: Future, C: AsyncClosure<Fut>> {
    state: FutureState<Fut, C>,
    completeness_holder: CompletenessHolder,
    _phantom_pinned: PhantomPinned,
}

#[cfg(not(feature="unstable-async-drop"))]
mod sync_blocking {
    use crate::{AsyncClosure, ScopedFuture};
    use std::sync::Arc;
    use std::task::Wake;
    use std::thread::Thread;

    #[allow(dead_code)]
    struct Waker(Thread);

    impl Wake for Waker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }

    impl<Fut: Future, C: AsyncClosure<Fut>> ScopedFuture<Fut, C> {
        pub fn block_sync_thread(&mut self) {
            if self.completeness_holder.is_completed() {
                return;
            }

            let thread = std::thread::current();
            let waker = Arc::new(Waker(thread));
            self.completeness_holder.waker.lock().unwrap().replace(waker.into());

            while !self.completeness_holder.is_completed() {
                std::thread::park();
            }
        }
    }
}

impl<Fut: Future, C: AsyncClosure<Fut>> Future for ScopedFuture<Fut, C> {
    type Output = <<C as AsyncClosure<Fut>>::OutputFuture as Future>::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: All below operations will be in-place
        let this = unsafe { self.get_unchecked_mut() };
        match &mut this.state {
            FutureState::NotEntered(closure) => {
                // SAFETY: This is cast &'a self.completeness_holder to &'static self.completeness_holder
                // As horrific as it looks it should be sound because made refrence always lives shorter than borrowed data which is Pinned, in drop we make sure to wait for all refrences to join, they never leak via public api
                let static_ref: &'static CompletenessHolder = unsafe { (&this.completeness_holder as *const CompletenessHolder).cast::<CompletenessHolder>().as_ref().unwrap() };
                let fut = closure.take().unwrap().call(static_ref);
                this.state = FutureState::Pending(fut);

                // SAFETY: reconstructing self for polling again
                let pinned = unsafe { Pin::new_unchecked(this) };
                pinned.poll(cx)
            }
            FutureState::Pending(fut) => {
                // SAFETY: If Self is pinned then we can assume that self.state is also Pinned because Self is !Unpin
                let pinned = unsafe { Pin::new_unchecked(fut) };
                match pinned.poll(cx) {
                    Poll::Ready(result) => {
                        if this.completeness_holder.is_completed() {
                            Poll::Ready(result)
                        } else {
                            this.state = FutureState::Completed(Some(result));
                            // SAFETY: reconstructing self for polling again
                            let pinned = unsafe { Pin::new_unchecked(this) };
                            pinned.poll(cx)
                        }
                    },
                    Poll::Pending => {
                        Poll::Pending
                    }
                }
            }
            FutureState::Completed(result) => {
                if this.completeness_holder.is_completed() {
                    Poll::Ready(result.take().unwrap())
                } else {
                    this.completeness_holder.waker.lock().unwrap().replace(cx.waker().clone());
                    Poll::Pending
                }
            }
        }
    }
}

impl<Fut: Future, C: AsyncClosure<Fut>> Drop for ScopedFuture<Fut, C> {
    fn drop(&mut self) {
        if !self.completeness_holder.is_completed() {
            #[cfg(feature="unstable-async-drop")]
            {
                let child_task_count = self.completeness_holder.ref_count.load(std::sync::atomic::Ordering::Relaxed);
                panic!("ScopedFuture can't be Dropped without running AsyncDrop while having {} child tasks still running", child_task_count);
            }

            #[cfg(not(feature="unstable-async-drop"))]
            {
                println!("Blocking main thread in ScopedFuture Drop");
                self.block_sync_thread()
            }
        }
    }
}

// Api for bridging this crate with other runtimes
pub trait TaskSpawner {

    // SAFETY: Not all runtimes let you spawn non static future (like tokio)
    // that's why it on this lib to make sure it's safe how we use it
    unsafe fn spawn<R: Send + 'static, F: Future<Output=R> + Send>(future: F) -> impl Future<Output = R>;
}

pub trait Spawnable {}

impl Spawnable for Enter {}
impl<'a, F: Future> Spawnable for F {}

#[derive(Clone)]
pub struct Guard<'a> {
    completeness_holder: &'a CompletenessHolder,
}

impl<'a> Guard<'a> {
    fn new(completeness_holder: &'a CompletenessHolder) -> Self {
        Self {
            completeness_holder
        }
    }
}

pub struct Scope<'a, Env, Spawner: TaskSpawner> {
    env: Env,
    guard: Guard<'a>,
    _spawner: PhantomData<Spawner>,
    _phantom_pinned: PhantomPinned,
}

impl<'a, Env, Spawner: TaskSpawner> Scope<'a, Env, Spawner> {
    fn new(env: Env, guard: Guard<'a>) -> Self {
        Self {
            env,
            guard,
            _spawner: Default::default(),
            _phantom_pinned: Default::default(),
        }
    }
}

pub struct Enter;
pub struct Finish<R> {
    result: R
}

impl<R> Finish<R> {
    fn new(result: R) -> Self {
        Self { result }
    }
}

impl From<Enter> for Finish<()> {
    fn from(_: Enter) -> Self {
        Self::new(())
    }
}

impl<'a, Spawner: TaskSpawner> Scope<'a, Enter, Spawner> {
    fn enter(guard: Guard<'a>) -> Self {
        Self::new(Enter, guard)
    }

    #[allow(dead_code)]
    fn finish<R>(self, value: R) -> Scope<'a, Finish<R>, Spawner> {
        Scope::new(Finish::new(value), self.guard)
    }
}

impl<'a, Env: Spawnable + Future, Spawner: TaskSpawner> Scope<'a, Env, Spawner> {
    pub fn with<R: Send + 'static, F: Future<Output=R> + Send>(self, future: F) -> Scope<'a, impl Future<Output=(Env::Output, R)>, Spawner>  {
        let guard_clone = self.guard.clone();
        let Self { env, guard, .. } = self;
        let task = Self::make_guarded_task(guard_clone, future);
        let handle = futures::future::join(env, task);
        Scope::new(handle, guard)
    }
}

impl<'a, Env: Spawnable, Spawner: TaskSpawner> Scope<'a, Env, Spawner> {
    pub fn spawn<R: Send + 'static, F: Future<Output=R> + Send>(self, future: F) -> Scope<'a, impl Future<Output=(Self, R)>, Spawner>  {
        let (g1, g2) = (self.guard.clone(), self.guard.clone());
        let task = Self::make_guarded_task(g1, future);
        let handle = async move {
            // We never send "inner" scope to other threads, just make it look like it was send
            (self, task.await)
        };

        Scope::new(handle, g2)
    }

    fn make_guarded_task<R: Send + 'static, F: Future<Output=R> + Send>(guard: Guard<'_>, future: F) -> impl Future<Output=R>  {
        let guard = guard.completeness_holder.make_guard();
        async move {

            // SAFETY: Rest of the module makes sure that spawning non static tasks should be sound
            // guard is made on parent thread, moved to child thread and parent thread will not "invalidate" parent future before child future
            // this is enforced by blocking on stable and yielding on nightly if we use unstable-async-drop feature
            // only after all of the guards are dropped parent future can also be dropped
            let guard = guard;
            unsafe {
                Spawner::spawn(async move {
                    let _guard = guard;
                    future.await
                }).await
            }
        }
    }

    // Well would be better if each future was standalone task
    /*fn spawn_all<R: Send + 'static, F: JoinableSendFuture<Output=R> + Send>(self, future: F) -> Scope<'a, impl Future<Output=(Self, R)>, Spawner> {
        self.spawn(future.join())
    }*/
}

/*pub trait JoinableSendFuture: Send {
    type Output: Send;
    fn join(self) -> impl Future<Output = Self::Output> + Send;
}

macro_rules! impl_variadic_joinable_future {
    ( $($name:ident, $f_n:literal )+ ) => {
        impl<$($name: Future + Send),+> JoinableSendFuture for ($($name,)+) where $($name::Output: Send, )+ {
            type Output = ($($name::Output,)+);

            fn join(self) -> impl Future<Output=Self::Output> {
                async move {
                    futures::join!($(self.$f_n,)+)
                }
            }
        }
    }
}

impl_variadic_joinable_future! { F0, 0 F1, 1  }
impl_variadic_joinable_future! { F0, 0 F1, 1 F2, 2  }
impl_variadic_joinable_future! { F0, 0 F1, 1 F2, 2 F3, 3  }
impl_variadic_joinable_future! { F0, 0 F1, 1 F2, 2 F3, 3 F4, 4  }
impl_variadic_joinable_future! { F0, 0 F1, 1 F2, 2 F3, 3 F4, 4 F5, 5  }
impl_variadic_joinable_future! { F0, 0 F1, 1 F2, 2 F3, 3 F4, 4 F5, 5 F6, 6  }
impl_variadic_joinable_future! { F0, 0 F1, 1 F2, 2 F3, 3 F4, 4 F5, 5 F6, 6 F7, 7  }*/

impl<'a, Env, Spawner: TaskSpawner> Future for Scope<'a, Env, Spawner>
    where Env: Future
{
    type Output = Env::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: Because Scope is pinned here and we know that is also !Unpin (because it contain PhantomPinned)
        // cast Pin<&mut Self> -> Pin<&mut self.env> should be sound
        let pinned = unsafe { Pin::new_unchecked(&mut self.get_unchecked_mut().env) };
        pinned.poll(cx)
    }
}

pub fn generic_scope<Spawner: TaskSpawner, R>(closure: impl for<'a> AsyncFnOnce(Scope<'a, Enter, Spawner>) -> Scope<'a, Finish<R>, Spawner> + 'static) -> impl Future<Output=R> {
    let closure = async move |guard_ref| {
        let guard = Guard::new(guard_ref);
        let scope = Scope::enter(guard);
        closure(scope).await.env.result
    };

    let ret = ScopedFuture {
        state: FutureState::NotEntered(Some(closure)),
        completeness_holder: Default::default(),
        _phantom_pinned: Default::default(),
    };

    ret
}

#[cfg(feature="tokio-spawner")]
pub mod tokio_spawner {
    use crate::{generic_scope, Enter};

    pub async fn scope<R>(closure: impl for<'a> AsyncFnOnce(Scope<'a, Enter, TokioSpawner>) -> Scope<'a, Finish<R>, TokioSpawner> + 'static) -> R {
        generic_scope::<TokioSpawner, R>(closure).await
    }

    use crate::{Finish, Scope, TaskSpawner};

    pub struct TokioSpawner;

    impl TaskSpawner for TokioSpawner {
        // SAFETY: fearfully unsafe, idk if sound, depends on tokio behavior
        // But as long as tokio will fulfill it's part of contract then it "should" be sound
        // It's exactly the same what std::thread::scope does
        unsafe fn spawn<R: Send + 'static, F: Future<Output=R> + Send>(future: F) -> impl Future<Output=R> {

            // SAFETY: Putting Future into box and casting Box<dyn Future + 'a> to Box<dyn Future + 'static>
            // As long as all captured data lives longer (and is never moved) than this Future then it's sound
            // Via Pinning, !Unpin and guards we make sure that this contract is upheld
            let pinned = Box::new(future);
            let dyn_static_future =
                unsafe { Box::from_raw(Box::into_raw(pinned) as *mut (dyn Future<Output=R> + Send + 'static)) };

            async move {
                tokio::spawn(Box::into_pin(dyn_static_future)).await.unwrap()
            }
        }
    }
}

#[cfg(test)]
mod examples {
    use crate::generic_scope;
    use crate::tokio_spawner::TokioSpawner;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn it_works() {
        let mut vec = vec![1, 2, 3];

        let vec = generic_scope::<TokioSpawner, _>(async move |scope| {
            let (scope, result) = scope.spawn(async {
                vec.push(0);
                vec.len() as i32 + 1
            }).await;

            vec.push(result);

            let scope_2 = scope.spawn(async {
                vec.len() as i32 + 1
            });

            let (scope_2, _) = scope_2.spawn(async {
                vec.len() as i32 + 1
            }).await;


            let (scope, result) = scope_2.await;

            vec.push(result);

            scope.finish(vec)
        }).await;

        println!("{:?}", vec);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn std_thread_scope_example_sequential() {
        // We cant share data not owned by the scope (but ofc we can move it in there)
        //let mut a = vec![1, 2, 3];
        //let mut x = 0;

        let (mut a, x) = pinned_scope::tokio_spawner::scope(async |s| { //async closure instead of normal one
            let a = vec![1, 2, 3];
            let mut x = 0;

            let (s, _) = s.spawn(async { // normal async blocks that can capture scope data
                println!("hello from the first scoped task");
                // We can borrow `a` here.
                dbg!(&a);
            }).await; // everytime task is spawned current scope guard is consumed

            let (s, _) = s.spawn(async {
                println!("hello from the second scoped task");
                // We can even mutably borrow `x` here,
                // because no other task is using it.
                x += a[0] + a[2];
            }).await;

            println!("hello from the main thread");
            s.finish((a, x)) // Closure must return guard: Scope<'_, Finish<_>, _>
        }).await;

        // After the scope, we can modify and access returned variables:
        a.push(4);
        assert_eq!(x, a.len());
    }

    use crate as pinned_scope;
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn std_thread_scope_example_concurrent() {
        // We cant share data not owned by the scope (but ofc we can move it in there)
        let a = vec![1, 2, 3];
        let mut x = 0;

        let (mut a, x) = pinned_scope::tokio_spawner::scope(async move |s| { //async closure instead of normal one
            let s = s.spawn(async { // normal async blocks that can capture scope data
                println!("hello from the first scoped task");
                // We can borrow `a` here.
                dbg!(&a);
            }); // lack of await therefore task is not started

            let ((s, _), _) = s.with(async {
                println!("hello from the second scoped task");
                // We can even mutably borrow `x` here,
                // because no other task is using it.
                x += a[0] + a[2];
            }).await; // await starts both tasks in unordered manner (uses futures::future::join)

            println!("hello from the main thread");
            s.finish((a, x)) // Closure must return guard: Scope<'_, Finish<_>, _>
        }).await;

        // After the scope, we can modify and access returned variables:
        a.push(4);
        assert_eq!(x, a.len());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn limitations() {

        async fn get_data() -> String {
            "requested data".to_string()
        }

        async fn process_data(data: &str) {
            println!("Data: \"{}\" processed", data);
        }

        async fn save_data(data: &str) {
            println!("Data: \"{}\" saved", data);
        }

        let data = get_data().await;

        /*
        // Not possible because scope cannot borrow data from outside
        async fn manage_data(data: &str) {
            pinned_scope::tokio_spawner::scope(async move |s| {
                let s = s.spawn(process_data(data));
                let ((s, _), _) = s.with(save_data(data)).await;
                s.finish(())
            }).await;
        }

        manage_data(&data).await;
        */

        // But it's okey to move data into and out of it
        async fn manage_data(data: String) -> String {
            pinned_scope::tokio_spawner::scope(async move |s| {
                // Or even get data directly inside
                // let data = get_data().await;
                let s = s.spawn(process_data(&data));
                let ((s, _), _) = s.with(save_data(&data)).await;
                s.finish(data)
            }).await
        }

        let _data = manage_data(data).await;
        // And still own data to do something else with it
    }
}
