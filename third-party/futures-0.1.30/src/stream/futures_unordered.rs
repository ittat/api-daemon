//! An unbounded set of futures.

use std::cell::UnsafeCell;
use std::fmt::{self, Debug};
use std::iter::FromIterator;
use std::marker::PhantomData;
use std::mem;
use std::ptr;
use std::sync::atomic::Ordering::{Relaxed, SeqCst, Acquire, Release, AcqRel};
use std::sync::atomic::{AtomicPtr, AtomicBool};
use std::sync::{Arc, Weak};
use std::usize;

use {task, Stream, Future, Poll, Async};
use executor::{Notify, UnsafeNotify, NotifyHandle};
use task_impl::{self, AtomicTask};

/// Constant used for a `FuturesUnordered` to determine how many times it is
/// allowed to poll underlying futures without yielding.
///
/// A single call to `poll_next` may potentially do a lot of work before
/// yielding. This happens in particular if the underlying futures are awoken
/// frequently but continue to return `Pending`. This is problematic if other
/// tasks are waiting on the executor, since they do not get to run. This value
/// caps the number of calls to `poll` on underlying futures a single call to
/// `poll_next` is allowed to make.
///
/// The value itself is chosen somewhat arbitrarily. It needs to be high enough
/// that amortize wakeup and scheduling costs, but low enough that we do not
/// starve other tasks for long.
///
/// See also https://github.com/rust-lang/futures-rs/issues/2047.
const YIELD_EVERY: usize = 32;

/// An unbounded set of futures.
///
/// This "combinator" also serves a special function in this library, providing
/// the ability to maintain a set of futures that and manage driving them all
/// to completion.
///
/// Futures are pushed into this set and their realized values are yielded as
/// they are ready. This structure is optimized to manage a large number of
/// futures. Futures managed by `FuturesUnordered` will only be polled when they
/// generate notifications. This reduces the required amount of work needed to
/// coordinate large numbers of futures.
///
/// When a `FuturesUnordered` is first created, it does not contain any futures.
/// Calling `poll` in this state will result in `Ok(Async::Ready(None))` to be
/// returned. Futures are submitted to the set using `push`; however, the
/// future will **not** be polled at this point. `FuturesUnordered` will only
/// poll managed futures when `FuturesUnordered::poll` is called. As such, it
/// is important to call `poll` after pushing new futures.
///
/// If `FuturesUnordered::poll` returns `Ok(Async::Ready(None))` this means that
/// the set is currently not managing any futures. A future may be submitted
/// to the set at a later time. At that point, a call to
/// `FuturesUnordered::poll` will either return the future's resolved value
/// **or** `Ok(Async::NotReady)` if the future has not yet completed.
///
/// Note that you can create a ready-made `FuturesUnordered` via the
/// `futures_unordered` function in the `stream` module, or you can start with an
/// empty set with the `FuturesUnordered::new` constructor.
#[must_use = "streams do nothing unless polled"]
pub struct FuturesUnordered<F> {
    inner: Arc<Inner<F>>,
    len: usize,
    head_all: *const Node<F>,
}

unsafe impl<T: Send> Send for FuturesUnordered<T> {}
unsafe impl<T: Sync> Sync for FuturesUnordered<T> {}

// FuturesUnordered is implemented using two linked lists. One which links all
// futures managed by a `FuturesUnordered` and one that tracks futures that have
// been scheduled for polling. The first linked list is not thread safe and is
// only accessed by the thread that owns the `FuturesUnordered` value. The
// second linked list is an implementation of the intrusive MPSC queue algorithm
// described by 1024cores.net.
//
// When a future is submitted to the set a node is allocated and inserted in
// both linked lists. The next call to `poll` will (eventually) see this node
// and call `poll` on the future.
//
// Before a managed future is polled, the current task's `Notify` is replaced
// with one that is aware of the specific future being run. This ensures that
// task notifications generated by that specific future are visible to
// `FuturesUnordered`. When a notification is received, the node is scheduled
// for polling by being inserted into the concurrent linked list.
//
// Each node uses an `AtomicUsize` to track it's state. The node state is the
// reference count (the number of outstanding handles to the node) as well as a
// flag tracking if the node is currently inserted in the atomic queue. When the
// future is notified, it will only insert itself into the linked list if it
// isn't currently inserted.

#[allow(missing_debug_implementations)]
struct Inner<T> {
    // The task using `FuturesUnordered`.
    parent: AtomicTask,

    // Head/tail of the readiness queue
    head_readiness: AtomicPtr<Node<T>>,
    tail_readiness: UnsafeCell<*const Node<T>>,
    stub: Arc<Node<T>>,
}

struct Node<T> {
    // The future
    future: UnsafeCell<Option<T>>,

    // Next pointer for linked list tracking all active nodes
    next_all: UnsafeCell<*const Node<T>>,

    // Previous node in linked list tracking all active nodes
    prev_all: UnsafeCell<*const Node<T>>,

    // Next pointer in readiness queue
    next_readiness: AtomicPtr<Node<T>>,

    // Queue that we'll be enqueued to when notified
    queue: Weak<Inner<T>>,

    // Whether or not this node is currently in the mpsc queue.
    queued: AtomicBool,
}

enum Dequeue<T> {
    Data(*const Node<T>),
    Empty,
    Inconsistent,
}

impl<T> Default for FuturesUnordered<T> where T: Future {
    fn default() -> Self {
        FuturesUnordered::new()
    }
}

impl<T> FuturesUnordered<T>
    where T: Future,
{
    /// Constructs a new, empty `FuturesUnordered`
    ///
    /// The returned `FuturesUnordered` does not contain any futures and, in this
    /// state, `FuturesUnordered::poll` will return `Ok(Async::Ready(None))`.
    pub fn new() -> FuturesUnordered<T> {
        let stub = Arc::new(Node {
            future: UnsafeCell::new(None),
            next_all: UnsafeCell::new(ptr::null()),
            prev_all: UnsafeCell::new(ptr::null()),
            next_readiness: AtomicPtr::new(ptr::null_mut()),
            queued: AtomicBool::new(true),
            queue: Weak::new(),
        });
        let stub_ptr = &*stub as *const Node<T>;
        let inner = Arc::new(Inner {
            parent: AtomicTask::new(),
            head_readiness: AtomicPtr::new(stub_ptr as *mut _),
            tail_readiness: UnsafeCell::new(stub_ptr),
            stub: stub,
        });

        FuturesUnordered {
            len: 0,
            head_all: ptr::null_mut(),
            inner: inner,
        }
    }
}

impl<T> FuturesUnordered<T> {
    /// Returns the number of futures contained in the set.
    ///
    /// This represents the total number of in-flight futures.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if the set contains no futures
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Push a future into the set.
    ///
    /// This function submits the given future to the set for managing. This
    /// function will not call `poll` on the submitted future. The caller must
    /// ensure that `FuturesUnordered::poll` is called in order to receive task
    /// notifications.
    pub fn push(&mut self, future: T) {
        let node = Arc::new(Node {
            future: UnsafeCell::new(Some(future)),
            next_all: UnsafeCell::new(ptr::null_mut()),
            prev_all: UnsafeCell::new(ptr::null_mut()),
            next_readiness: AtomicPtr::new(ptr::null_mut()),
            queued: AtomicBool::new(true),
            queue: Arc::downgrade(&self.inner),
        });

        // Right now our node has a strong reference count of 1. We transfer
        // ownership of this reference count to our internal linked list
        // and we'll reclaim ownership through the `unlink` function below.
        let ptr = self.link(node);

        // We'll need to get the future "into the system" to start tracking it,
        // e.g. getting its unpark notifications going to us tracking which
        // futures are ready. To do that we unconditionally enqueue it for
        // polling here.
        self.inner.enqueue(ptr);
    }

    /// Returns an iterator that allows modifying each future in the set.
    pub fn iter_mut(&mut self) -> IterMut<T> {
        IterMut {
            node: self.head_all,
            len: self.len,
            _marker: PhantomData
        }
    }

    fn release_node(&mut self, node: Arc<Node<T>>) {
        // The future is done, try to reset the queued flag. This will prevent
        // `notify` from doing any work in the future
        let prev = node.queued.swap(true, SeqCst);

        // Drop the future, even if it hasn't finished yet. This is safe
        // because we're dropping the future on the thread that owns
        // `FuturesUnordered`, which correctly tracks T's lifetimes and such.
        unsafe {
            drop((*node.future.get()).take());
        }

        // If the queued flag was previously set then it means that this node
        // is still in our internal mpsc queue. We then transfer ownership
        // of our reference count to the mpsc queue, and it'll come along and
        // free it later, noticing that the future is `None`.
        //
        // If, however, the queued flag was *not* set then we're safe to
        // release our reference count on the internal node. The queued flag
        // was set above so all future `enqueue` operations will not actually
        // enqueue the node, so our node will never see the mpsc queue again.
        // The node itself will be deallocated once all reference counts have
        // been dropped by the various owning tasks elsewhere.
        if prev {
            mem::forget(node);
        }
    }

    /// Insert a new node into the internal linked list.
    fn link(&mut self, node: Arc<Node<T>>) -> *const Node<T> {
        let ptr = arc2ptr(node);
        unsafe {
            *(*ptr).next_all.get() = self.head_all;
            if !self.head_all.is_null() {
                *(*self.head_all).prev_all.get() = ptr;
            }
        }

        self.head_all = ptr;
        self.len += 1;
        return ptr
    }

    /// Remove the node from the linked list tracking all nodes currently
    /// managed by `FuturesUnordered`.
    unsafe fn unlink(&mut self, node: *const Node<T>) -> Arc<Node<T>> {
        let node = ptr2arc(node);
        let next = *node.next_all.get();
        let prev = *node.prev_all.get();
        *node.next_all.get() = ptr::null_mut();
        *node.prev_all.get() = ptr::null_mut();

        if !next.is_null() {
            *(*next).prev_all.get() = prev;
        }

        if !prev.is_null() {
            *(*prev).next_all.get() = next;
        } else {
            self.head_all = next;
        }
        self.len -= 1;
        return node
    }
}

impl<T> Stream for FuturesUnordered<T>
    where T: Future
{
    type Item = T::Item;
    type Error = T::Error;

    fn poll(&mut self) -> Poll<Option<T::Item>, T::Error> {
        // Keep track of how many child futures we have polled,
        // in case we want to forcibly yield.
        let mut polled = 0;

        // Ensure `parent` is correctly set.
        self.inner.parent.register();

        loop {
            let node = match unsafe { self.inner.dequeue() } {
                Dequeue::Empty => {
                    if self.is_empty() {
                        return Ok(Async::Ready(None));
                    } else {
                        return Ok(Async::NotReady)
                    }
                }
                Dequeue::Inconsistent => {
                    // At this point, it may be worth yielding the thread &
                    // spinning a few times... but for now, just yield using the
                    // task system.
                    task::current().notify();
                    return Ok(Async::NotReady);
                }
                Dequeue::Data(node) => node,
            };

            debug_assert!(node != self.inner.stub());

            unsafe {
                let mut future = match (*(*node).future.get()).take() {
                    Some(future) => future,

                    // If the future has already gone away then we're just
                    // cleaning out this node. See the comment in
                    // `release_node` for more information, but we're basically
                    // just taking ownership of our reference count here.
                    None => {
                        let node = ptr2arc(node);
                        assert!((*node.next_all.get()).is_null());
                        assert!((*node.prev_all.get()).is_null());
                        continue
                    }
                };

                // Unset queued flag... this must be done before
                // polling. This ensures that the future gets
                // rescheduled if it is notified **during** a call
                // to `poll`.
                let prev = (*node).queued.swap(false, SeqCst);
                assert!(prev);

                // We're going to need to be very careful if the `poll`
                // function below panics. We need to (a) not leak memory and
                // (b) ensure that we still don't have any use-after-frees. To
                // manage this we do a few things:
                //
                // * This "bomb" here will call `release_node` if dropped
                //   abnormally. That way we'll be sure the memory management
                //   of the `node` is managed correctly.
                // * The future was extracted above (taken ownership). That way
                //   if it panics we're guaranteed that the future is
                //   dropped on this thread and doesn't accidentally get
                //   dropped on a different thread (bad).
                // * We unlink the node from our internal queue to preemptively
                //   assume it'll panic, in which case we'll want to discard it
                //   regardless.
                struct Bomb<'a, T: 'a> {
                    queue: &'a mut FuturesUnordered<T>,
                    node: Option<Arc<Node<T>>>,
                }
                impl<'a, T> Drop for Bomb<'a, T> {
                    fn drop(&mut self) {
                        if let Some(node) = self.node.take() {
                            self.queue.release_node(node);
                        }
                    }
                }
                let mut bomb = Bomb {
                    node: Some(self.unlink(node)),
                    queue: self,
                };

                // Poll the underlying future with the appropriate `notify`
                // implementation. This is where a large bit of the unsafety
                // starts to stem from internally. The `notify` instance itself
                // is basically just our `Arc<Node<T>>` and tracks the mpsc
                // queue of ready futures.
                //
                // Critically though `Node<T>` won't actually access `T`, the
                // future, while it's floating around inside of `Task`
                // instances. These structs will basically just use `T` to size
                // the internal allocation, appropriately accessing fields and
                // deallocating the node if need be.
                let res = {
                    let notify = NodeToHandle(bomb.node.as_ref().unwrap());
                    task_impl::with_notify(&notify, 0, || {
                        future.poll()
                    })
                };
                polled += 1;

                let ret = match res {
                    Ok(Async::NotReady) => {
                        let node = bomb.node.take().unwrap();
                        *node.future.get() = Some(future);
                        bomb.queue.link(node);

                        if polled == YIELD_EVERY {
                            // We have polled a large number of futures in a row without yielding.
                            // To ensure we do not starve other tasks waiting on the executor,
                            // we yield here, but immediately wake ourselves up to continue.
                            task_impl::current().notify();
                            return Ok(Async::NotReady);
                        }
                        continue
                    }
                    Ok(Async::Ready(e)) => Ok(Async::Ready(Some(e))),
                    Err(e) => Err(e),
                };
                return ret
            }
        }
    }
}

impl<T: Debug> Debug for FuturesUnordered<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "FuturesUnordered {{ ... }}")
    }
}

impl<T> Drop for FuturesUnordered<T> {
    fn drop(&mut self) {
        // When a `FuturesUnordered` is dropped we want to drop all futures associated
        // with it. At the same time though there may be tons of `Task` handles
        // flying around which contain `Node<T>` references inside them. We'll
        // let those naturally get deallocated when the `Task` itself goes out
        // of scope or gets notified.
        unsafe {
            while !self.head_all.is_null() {
                let head = self.head_all;
                let node = self.unlink(head);
                self.release_node(node);
            }
        }

        // Note that at this point we could still have a bunch of nodes in the
        // mpsc queue. None of those nodes, however, have futures associated
        // with them so they're safe to destroy on any thread. At this point
        // the `FuturesUnordered` struct, the owner of the one strong reference
        // to `Inner<T>` will drop the strong reference. At that point
        // whichever thread releases the strong refcount last (be it this
        // thread or some other thread as part of an `upgrade`) will clear out
        // the mpsc queue and free all remaining nodes.
        //
        // While that freeing operation isn't guaranteed to happen here, it's
        // guaranteed to happen "promptly" as no more "blocking work" will
        // happen while there's a strong refcount held.
    }
}

impl<F: Future> FromIterator<F> for FuturesUnordered<F> {
    fn from_iter<T>(iter: T) -> Self 
        where T: IntoIterator<Item = F>
    {
        let mut new = FuturesUnordered::new();
        for future in iter.into_iter() {
            new.push(future);
        }
        new
    }
}

#[derive(Debug)]
/// Mutable iterator over all futures in the unordered set.
pub struct IterMut<'a, F: 'a> {
    node: *const Node<F>,
    len: usize,
    _marker: PhantomData<&'a mut FuturesUnordered<F>>
}

impl<'a, F> Iterator for IterMut<'a, F> {
    type Item = &'a mut F;

    fn next(&mut self) -> Option<&'a mut F> {
        if self.node.is_null() {
            return None;
        }
        unsafe {
            let future = (*(*self.node).future.get()).as_mut().unwrap();
            let next = *(*self.node).next_all.get();
            self.node = next;
            self.len -= 1;
            return Some(future);
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.len, Some(self.len))
    }
}

impl<'a, F> ExactSizeIterator for IterMut<'a, F> {}

impl<T> Inner<T> {
    /// The enqueue function from the 1024cores intrusive MPSC queue algorithm.
    fn enqueue(&self, node: *const Node<T>) {
        unsafe {
            debug_assert!((*node).queued.load(Relaxed));

            // This action does not require any coordination
            (*node).next_readiness.store(ptr::null_mut(), Relaxed);

            // Note that these atomic orderings come from 1024cores
            let node = node as *mut _;
            let prev = self.head_readiness.swap(node, AcqRel);
            (*prev).next_readiness.store(node, Release);
        }
    }

    /// The dequeue function from the 1024cores intrusive MPSC queue algorithm
    ///
    /// Note that this unsafe as it required mutual exclusion (only one thread
    /// can call this) to be guaranteed elsewhere.
    unsafe fn dequeue(&self) -> Dequeue<T> {
        let mut tail = *self.tail_readiness.get();
        let mut next = (*tail).next_readiness.load(Acquire);

        if tail == self.stub() {
            if next.is_null() {
                return Dequeue::Empty;
            }

            *self.tail_readiness.get() = next;
            tail = next;
            next = (*next).next_readiness.load(Acquire);
        }

        if !next.is_null() {
            *self.tail_readiness.get() = next;
            debug_assert!(tail != self.stub());
            return Dequeue::Data(tail);
        }

        if self.head_readiness.load(Acquire) as *const _ != tail {
            return Dequeue::Inconsistent;
        }

        self.enqueue(self.stub());

        next = (*tail).next_readiness.load(Acquire);

        if !next.is_null() {
            *self.tail_readiness.get() = next;
            return Dequeue::Data(tail);
        }

        Dequeue::Inconsistent
    }

    fn stub(&self) -> *const Node<T> {
        &*self.stub
    }
}

impl<T> Drop for Inner<T> {
    fn drop(&mut self) {
        // Once we're in the destructor for `Inner<T>` we need to clear out the
        // mpsc queue of nodes if there's anything left in there.
        //
        // Note that each node has a strong reference count associated with it
        // which is owned by the mpsc queue. All nodes should have had their
        // futures dropped already by the `FuturesUnordered` destructor above,
        // so we're just pulling out nodes and dropping their refcounts.
        unsafe {
            loop {
                match self.dequeue() {
                    Dequeue::Empty => break,
                    Dequeue::Inconsistent => abort("inconsistent in drop"),
                    Dequeue::Data(ptr) => drop(ptr2arc(ptr)),
                }
            }
        }
    }
}

#[allow(missing_debug_implementations)]
struct NodeToHandle<'a, T: 'a>(&'a Arc<Node<T>>);

impl<'a, T> Clone for NodeToHandle<'a, T> {
    fn clone(&self) -> Self {
        NodeToHandle(self.0)
    }
}

impl<'a, T> From<NodeToHandle<'a, T>> for NotifyHandle {
    fn from(handle: NodeToHandle<'a, T>) -> NotifyHandle {
        unsafe {
            let ptr = handle.0.clone();
            let ptr = mem::transmute::<Arc<Node<T>>, *mut ArcNode<T>>(ptr);
            NotifyHandle::new(hide_lt(ptr))
        }
    }
}

struct ArcNode<T>(PhantomData<T>);

// We should never touch `T` on any thread other than the one owning
// `FuturesUnordered`, so this should be a safe operation.
unsafe impl<T> Send for ArcNode<T> {}
unsafe impl<T> Sync for ArcNode<T> {}

impl<T> Notify for ArcNode<T> {
    fn notify(&self, _id: usize) {
        unsafe {
            let me: *const ArcNode<T> = self;
            let me: *const *const ArcNode<T> = &me;
            let me = me as *const Arc<Node<T>>;
            Node::notify(&*me)
        }
    }
}

unsafe impl<T> UnsafeNotify for ArcNode<T> {
    unsafe fn clone_raw(&self) -> NotifyHandle {
        let me: *const ArcNode<T> = self;
        let me: *const *const ArcNode<T> = &me;
        let me = &*(me as *const Arc<Node<T>>);
        NodeToHandle(me).into()
    }

    unsafe fn drop_raw(&self) {
        let mut me: *const ArcNode<T> = self;
        let me = &mut me as *mut *const ArcNode<T> as *mut Arc<Node<T>>;
        ptr::drop_in_place(me);
    }
}

unsafe fn hide_lt<T>(p: *mut ArcNode<T>) -> *mut UnsafeNotify {
    mem::transmute(p as *mut UnsafeNotify)
}

impl<T> Node<T> {
    fn notify(me: &Arc<Node<T>>) {
        let inner = match me.queue.upgrade() {
            Some(inner) => inner,
            None => return,
        };

        // It's our job to notify the node that it's ready to get polled,
        // meaning that we need to enqueue it into the readiness queue. To
        // do this we flag that we're ready to be queued, and if successful
        // we then do the literal queueing operation, ensuring that we're
        // only queued once.
        //
        // Once the node is inserted we be sure to notify the parent task,
        // as it'll want to come along and pick up our node now.
        //
        // Note that we don't change the reference count of the node here,
        // we're just enqueueing the raw pointer. The `FuturesUnordered`
        // implementation guarantees that if we set the `queued` flag true that
        // there's a reference count held by the main `FuturesUnordered` queue
        // still.
        let prev = me.queued.swap(true, SeqCst);
        if !prev {
            inner.enqueue(&**me);
            inner.parent.notify();
        }
    }
}

impl<T> Drop for Node<T> {
    fn drop(&mut self) {
        // Currently a `Node<T>` is sent across all threads for any lifetime,
        // regardless of `T`. This means that for memory safety we can't
        // actually touch `T` at any time except when we have a reference to the
        // `FuturesUnordered` itself.
        //
        // Consequently it *should* be the case that we always drop futures from
        // the `FuturesUnordered` instance, but this is a bomb in place to catch
        // any bugs in that logic.
        unsafe {
            if (*self.future.get()).is_some() {
                abort("future still here when dropping");
            }
        }
    }
}

fn arc2ptr<T>(ptr: Arc<T>) -> *const T {
    let addr = &*ptr as *const T;
    mem::forget(ptr);
    return addr
}

unsafe fn ptr2arc<T>(ptr: *const T) -> Arc<T> {
    let anchor = mem::transmute::<usize, Arc<T>>(0x10);
    let addr = &*anchor as *const T;
    mem::forget(anchor);
    let offset = addr as isize - 0x10;
    mem::transmute::<isize, Arc<T>>(ptr as isize - offset)
}

fn abort(s: &str) -> ! {
    struct DoublePanic;

    impl Drop for DoublePanic {
        fn drop(&mut self) {
            panic!("panicking twice to abort the program");
        }
    }

    let _bomb = DoublePanic;
    panic!("{}", s);
}