use futures_util::future::poll_fn;
use pin_project_lite::pin_project;

use super::{ctx::WakerCtx, TreeStack};
use crate::{stack::State, Stack};
use std::{
    cell::UnsafeCell,
    future::Future,
    marker::PhantomData,
    pin::{pin, Pin},
    ptr::NonNull,
    sync::Arc,
    task::{ready, Context, Poll},
};

pin_project! {
    #[must_use]
    pub struct StkFuture<'a, F, R> {
        #[pin]
        place: UnsafeCell<Option<R>>,
        entry: Option<F>,
            _marker: PhantomData<&'a Stk>,
    }
}

pub struct ScopeFuture<'a, F, R> {
    entry: Option<F>,
    place: Arc<UnsafeCell<Option<R>>>,
    _marker: PhantomData<&'a Stk>,
}

impl<'a, F, R> ScopeFuture<'a, F, R> {
    pin_utils::unsafe_unpinned!(entry: Option<F>);
}

impl<'a, F, Fut, R> Future for ScopeFuture<'a, F, R>
where
    F: FnOnce(&'a ScopeStk) -> Fut,
    Fut: Future<Output = R> + 'a,
    R: 'a,
{
    type Output = R;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        if let Some(x) = this.entry.take() {
            let prev = WakerCtx::ptr_from_waker(cx.waker());
            let fut = unsafe { (x)(ScopeStk::new()) };
            let place = this.place.clone();
            unsafe {
                let waker = cx.waker().clone();
                prev.as_ref().fan.push(Box::pin(async move {
                    let mut waker = Some(waker);
                    let mut pin_future = pin!(fut);
                    poll_fn(|cx: &mut Context| {
                        let res = ready!(pin_future.as_mut().poll(cx));
                        place.get().write(Some(res));
                        waker.take().unwrap().wake();
                        Poll::Ready(())
                    })
                    .await
                }));
            }
            Poll::Pending
        } else {
            if let Some(x) = unsafe { std::ptr::replace(this.place.get(), None) } {
                Poll::Ready(x)
            } else {
                Poll::Pending
            }
        }
    }
}

#[must_use]
pub struct YieldFuture {
    done: bool,
}

/// A refernce back to stack from inside the running future.
///
/// Used for spawning new futures onto the stack from a future running on the stack.
pub struct Stk(PhantomData<*mut TreeStack>);

impl Stk {
    pub(super) unsafe fn new() -> &'static mut Self {
        NonNull::dangling().as_mut()
    }
}

impl Stk {
    /// Run a new future in the runtime.
    pub fn run<'a, F, Fut, R>(&'a mut self, f: F) -> StkFuture<'a, F, R>
    where
        F: FnOnce(&'a mut Stk) -> Fut,
        Fut: Future<Output = R> + 'a,
    {
        StkFuture {
            place: UnsafeCell::new(None),
            entry: Some(f),
            _marker: PhantomData,
        }
    }

    /// Yield the execution of the recursive futures back to the reblessive runtime.
    ///
    /// When stepping through a function instead of finishing it awaiting the future returned by
    /// this function will cause the the current step to complete.
    pub fn yield_now(&mut self) -> YieldFuture {
        YieldFuture { done: false }
    }

    pub fn scope<'a, F, Fut, R>(&'a mut self, f: F) -> ScopeFuture<'a, F, R>
    where
        F: FnOnce(&'a ScopeStk) -> Fut,
        Fut: Future<Output = R> + 'a,
    {
        ScopeFuture {
            entry: Some(f),
            place: Arc::new(UnsafeCell::new(None)),
            _marker: PhantomData,
        }
    }
}

#[must_use]
pub struct ScopeStkFuture<'a, R> {
    place: NonNull<UnsafeCell<Option<R>>>,
    stack: Stack,
    _marker: PhantomData<&'a ScopeStk>,
}

impl<'a, R> Drop for ScopeStkFuture<'a, R> {
    fn drop(&mut self) {
        unsafe { std::mem::drop(Box::from_raw(self.place.as_ptr())) };
    }
}

impl<'a, R> ScopeStkFuture<'a, R> {
    pin_utils::unsafe_unpinned!(stack: Stack);
    pin_utils::unsafe_unpinned!(place: NonNull<UnsafeCell<Option<R>>>);
}

impl<'a, R> Future for ScopeStkFuture<'a, R> {
    type Output = R;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let prev = WakerCtx::ptr_from_waker(cx.waker());
        let context = WakerCtx {
            waker: cx.waker(),
            stack: &self.stack,
            fan: unsafe { prev.as_ref().fan },
        };
        let waker = unsafe { context.to_waker() };
        let mut cx = Context::from_waker(&waker);

        loop {
            match self.stack.drive_head(&mut cx) {
                Poll::Pending => {
                    return Poll::Pending;
                }
                Poll::Ready(_) => match self.stack.get_state() {
                    State::Base => {
                        if let Some(x) = unsafe { (*self.place().as_ref().get()).take() } {
                            return Poll::Ready(x);
                        }
                        return Poll::Pending;
                    }
                    State::NewTask => self.stack.set_state(State::Base),
                    State::Yield => todo!(),
                },
            }
        }
    }
}

/// A refernce back to stack from inside the running future.
///
/// Used for spawning new futures onto the stack from a future running on the stack.
pub struct ScopeStk {
    marker: PhantomData<*mut TreeStack>,
}

impl ScopeStk {
    pub(super) unsafe fn new() -> &'static mut Self {
        NonNull::dangling().as_mut()
    }
}

impl ScopeStk {
    /// Run a new future in the runtime.
    pub fn run<'a, F, Fut, R>(&'a self, f: F) -> ScopeStkFuture<'a, R>
    where
        F: FnOnce(&'a mut Stk) -> Fut,
        Fut: Future<Output = R> + 'a,
    {
        let stack = Stack::new();
        let place = Box::into_raw(Box::new(UnsafeCell::new(None)));
        let place = unsafe { NonNull::new_unchecked(place) };
        let future = unsafe { f(Stk::new()) };

        stack.tasks().push(async move {
            let mut pin_future = pin!(future);
            poll_fn(move |cx: &mut Context| -> Poll<()> {
                let res = ready!(pin_future.as_mut().poll(cx));
                unsafe { place.as_ref().get().write(Some(res)) };
                Poll::Ready(())
            })
            .await
        });

        ScopeStkFuture {
            place,
            stack,
            _marker: PhantomData,
        }
    }
}

/*
struct WakeOnDrop(Option<Waker>);

impl Drop for WakeOnDrop {
    fn drop(&mut self) {
        self.0.take().wake();
    }
}

#[derive(Clone)]
struct ArcWaker(Arc<WakeOnDrop>);

impl ArcWaker{
    pub fn new(waker: Waker){
        Arc::new(WakeOnDrop(Some(waker)));
    }
}
*/
