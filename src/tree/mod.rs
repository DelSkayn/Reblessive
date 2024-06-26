pub use crate::stack::YieldFuture;
use crate::{
    defer::Defer,
    stack::{enter_stack_context, State},
    Stack,
};
use std::{
    cell::{Cell, UnsafeCell},
    future::Future,
    marker::PhantomData,
    pin::Pin,
    ptr::NonNull,
    task::{Context, Poll},
};

mod schedular;
use schedular::Schedular;

mod stk;
pub use stk::{ScopeFuture, Stk, StkFuture};

#[cfg(test)]
mod test;

#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct FinishFuture<'a, R> {
    runner: Runner<'a, R>,
}

impl<'a, R> Future for FinishFuture<'a, R> {
    type Output = R;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        enter_stack_context(&self.runner.ptr.root, || {
            let ptr = self.runner.ptr.root.set_context(NonNull::from(&*cx).cast());
            let defer = Defer::new(self.runner.ptr, |schedular| {
                schedular.root.set_context(ptr);
            });

            enter_tree_context(&self.runner.ptr.fanout, || {
                loop {
                    // First we need finish all fanout futures.
                    while !defer.fanout.is_empty() {
                        if unsafe { defer.fanout.poll(cx) }.is_pending() {
                            return Poll::Pending;
                        }
                    }

                    // No futures left in fanout, run on the root stack.
                    match defer.root.drive_head(cx) {
                        Poll::Ready(_) => {
                            if defer.root.tasks().is_empty() {
                                unsafe {
                                    return Poll::Ready(
                                        (*self.runner.place.as_ref().get()).take().unwrap(),
                                    );
                                }
                            }
                        }
                        Poll::Pending => match defer.root.get_state() {
                            State::Base => {
                                if defer.fanout.is_empty() {
                                    return Poll::Pending;
                                }
                            }
                            State::Cancelled => unreachable!("TreeStack dropped while stepping"),
                            State::NewTask | State::Yield => {
                                defer.root.set_state(State::Base);
                            }
                        },
                    }
                }
            })
        })
    }
}

#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct StepFuture<'a, 'b, R> {
    runner: &'a mut Runner<'b, R>,
}

impl<'a, 'b, R> Future for StepFuture<'a, 'b, R> {
    type Output = Option<R>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        enter_stack_context(&self.runner.ptr.root, || {
            let ptr = self.runner.ptr.root.set_context(NonNull::from(&*cx).cast());
            let defer = Defer::new(self.runner.ptr, |schedular| {
                schedular.root.set_context(ptr);
            });

            enter_tree_context(&defer.fanout, || {
                if !defer.fanout.is_empty() {
                    if unsafe { defer.fanout.poll(cx) }.is_pending() {
                        return Poll::Pending;
                    }
                }

                // No futures left in fanout, run on the root stack.l
                match defer.root.drive_head(cx) {
                    Poll::Ready(_) => {
                        if defer.root.tasks().is_empty() {
                            unsafe {
                                return Poll::Ready(Some(
                                    (*self.runner.place.as_ref().get()).take().unwrap(),
                                ));
                            }
                        }
                    }
                    Poll::Pending => match defer.root.get_state() {
                        State::Base => {
                            if defer.fanout.is_empty() {
                                return Poll::Pending;
                            } else {
                                return Poll::Ready(None);
                            }
                        }
                        State::Cancelled => unreachable!("TreeStack dropped while stepping"),
                        State::NewTask | State::Yield => {
                            defer.root.set_state(State::Base);
                        }
                    },
                }
                Poll::Ready(None)
            })
        })
    }
}

pub struct Runner<'a, R> {
    place: NonNull<UnsafeCell<Option<R>>>,
    ptr: &'a TreeStack,
    _stack_marker: PhantomData<&'a mut TreeStack>,
}

unsafe impl<'a, R> Send for Runner<'a, R> {}
unsafe impl<'a, R> Sync for Runner<'a, R> {}

impl<'a, R> Runner<'a, R> {
    pub fn finish(self) -> FinishFuture<'a, R> {
        FinishFuture { runner: self }
    }

    pub fn step<'b>(&'b mut self) -> StepFuture<'b, 'a, R> {
        StepFuture { runner: self }
    }
}

impl<'a, R> Drop for Runner<'a, R> {
    fn drop(&mut self) {
        self.ptr.root.clear();
        self.ptr.fanout.clear();
        unsafe { std::mem::drop(Box::from_raw(self.place.as_ptr())) };
    }
}

thread_local! {
    static TREE_PTR: Cell<Option<NonNull<Schedular>>> = const { Cell::new(None) };
}

pub fn enter_tree_context<F, R>(ctx: &Schedular, f: F) -> R
where
    F: FnOnce() -> R,
{
    let ptr = TREE_PTR.with(|x| x.replace(Some(NonNull::from(ctx))));
    struct Dropper(Option<NonNull<Schedular>>);
    impl Drop for Dropper {
        fn drop(&mut self) {
            TREE_PTR.with(|x| x.set(self.0))
        }
    }
    let _dropper = Dropper(ptr);
    f()
}

pub fn with_tree_context<F, R>(f: F) -> R
where
    F: FnOnce(&Schedular) -> R,
{
    let ptr = TREE_PTR
        .with(|x| x.get())
        .expect("Not within a tree stack context");
    unsafe { f(ptr.as_ref()) }
}

pub struct TreeStack {
    root: Stack,
    fanout: Schedular,
}

unsafe impl Send for TreeStack {}
unsafe impl Sync for TreeStack {}

impl TreeStack {
    pub fn new() -> Self {
        TreeStack {
            root: Stack::new(),
            fanout: Schedular::new(),
        }
    }

    pub fn enter<'a, F, Fut, R>(&'a mut self, f: F) -> Runner<R>
    where
        F: FnOnce(&'a mut Stk) -> Fut,
        Fut: Future<Output = R> + 'a,
    {
        let future = unsafe { f(Stk::new()) };
        let place = Box::into_raw(Box::new(UnsafeCell::new(None)));
        let place = unsafe { NonNull::new_unchecked(place) };

        self.root.tasks().push(async move {
            unsafe {
                (*place.as_ref().get()) = Some(future.await);
            }
        });

        Runner {
            place,
            ptr: self,
            _stack_marker: PhantomData,
        }
    }
}

impl Default for TreeStack {
    fn default() -> Self {
        Self::new()
    }
}
