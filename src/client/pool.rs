use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io;
use std::ops::{Deref, DerefMut, BitAndAssign};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use futures::{Future, Async, Poll};
use relay;

use proto::{KeepAlive, KA};
use super::Ver;

pub(super) type Key = (Rc<String>, Ver);

pub struct Pool<T> {
    inner: Rc<RefCell<PoolInner<T>>>,
}

// Before using a pooled connection, make sure the sender is not dead.
//
// This is a trait to allow the `client::pool::tests` to work for `i32`.
//
// See https://github.com/hyperium/hyper/issues/1429
pub trait Ready {
    fn poll_ready(&mut self) -> Poll<(), ()>;
}

struct PoolInner<T> {
    #[cfg(feature = "http2")]
    connecting: HashMap<Key, ()>,
    enabled: bool,
    // These are internal Conns sitting in the event loop in the KeepAlive
    // state, waiting to receive a new Request to send on the socket.
    idle: HashMap<Key, Vec<Entry<T>>>,
    // These are outstanding Checkouts that are waiting for a socket to be
    // able to send a Request one. This is used when "racing" for a new
    // connection.
    //
    // The Client starts 2 tasks, 1 to connect a new socket, and 1 to wait
    // for the Pool to receive an idle Conn. When a Conn becomes idle,
    // this list is checked for any parked Checkouts, and tries to notify
    // them that the Conn could be used instead of waiting for a brand new
    // connection.
    parked: HashMap<Key, VecDeque<(relay::Sender<Entry<T>>, CancelToken)>>,
    timeout: Option<Duration>,
}

impl<T: Clone + Ready> Pool<T> {

    #[cfg(feature = "http2")]
    pub fn new(enabled: bool, timeout: Option<Duration>) -> Pool<T> {
        Pool {
            inner: Rc::new(RefCell::new(PoolInner {
                // field attributes are unstable on Rust 1.18
                //#[cfg(feature = "http2")]
                connecting: HashMap::new(),
                enabled: enabled,
                idle: HashMap::new(),
                parked: HashMap::new(),
                timeout: timeout,
            })),
        }
    }

    #[cfg(not(feature = "http2"))]
    pub fn new(enabled: bool, timeout: Option<Duration>) -> Pool<T> {
        Pool {
            inner: Rc::new(RefCell::new(PoolInner {
                enabled: enabled,
                idle: HashMap::new(),
                parked: HashMap::new(),
                timeout: timeout,
            })),
        }
    }

    pub(super) fn checkout(&self, key: &str, ver: Ver) -> Checkout<T> {
        Checkout {
            cancel_token: CancelToken(Rc::new(Cell::new(false))),
            key: (Rc::new(key.to_owned()), ver),
            pool: self.clone(),
            parked: None,
        }
    }

    #[cfg(feature = "http2")]
    pub(super) fn connecting(&self, key: Key) {
        if key.1 != Ver::Http1 {
            self.inner.borrow_mut().connecting.insert(key, ());
        }
    }

    #[cfg(feature = "http2")]
    pub(super) fn is_connecting(&self, key: &Key) -> bool {
        key.1 != Ver::Http1
            && self.inner.borrow().connecting.contains_key(key)
    }

    fn put(&self, key: Key, entry: Entry<T>) {
        trace!("Pool::put {:?}", key);
        let mut inner = self.inner.borrow_mut();
        let mut remove_parked = false;

        entry.status.set(TimedKA::Idle(Instant::now()));

        let mut entry = Some(entry);
        if let Some(parked) = inner.parked.get_mut(&key) {
            while let Some((tx, token)) = parked.pop_front() {
                if tx.is_canceled() || token.is_canceled() {
                    trace!("Pool::put removing canceled parked {:?}", key);
                } else {
                    if key.1 == Ver::Http1 {
                        tx.complete(entry.take().unwrap());
                        break;
                    } else {
                        tx.complete(entry.clone().take().unwrap());
                    }
                }
                /*
                match tx.send(entry.take().unwrap()) {
                    Ok(()) => break,
                    Err(e) => {
                        trace!("Pool::put removing canceled parked {:?}", key);
                        entry = Some(e);
                    }
                }
                */
            }
            remove_parked = parked.is_empty();
        }
        if remove_parked {
            inner.parked.remove(&key);
            #[cfg(feature = "http2")]
            {
                inner.connecting.remove(&key);
            }
        }

        match entry {
            Some(entry) => {
                debug!("pooling idle connection for {:?}", key);
                inner.idle.entry(key)
                     .or_insert(Vec::new())
                     .push(entry);
            }
            None => trace!("Pool::put found parked {:?}", key),
        }
    }

    fn take(&self, key: &Key) -> Option<Pooled<T>> {
        let entry = {
            let mut inner = self.inner.borrow_mut();
            let expiration = Expiration::new(inner.timeout);
            let mut should_remove = false;
            let entry = inner.idle.get_mut(key).and_then(|list| {
                trace!("take; url = {:?}, expiration = {:?}", key, expiration.0);
                while let Some(mut entry) = list.pop() {
                    match entry.status.get() {
                        TimedKA::Idle(idle_at) if !expiration.expires(idle_at) => {
                            if let Ok(Async::Ready(())) = entry.value.poll_ready() {
                                if key.1 != Ver::Http1 {
                                    entry.status.set(TimedKA::Idle(Instant::now()));
                                    list.push(entry.clone());
                                }
                                should_remove = list.is_empty();
                                return Some(entry);
                            }
                        },
                        _ => {},
                    }
                    trace!("removing unacceptable pooled {:?}", key);
                    // every other case the Entry should just be dropped
                    // 1. Idle but expired
                    // 2. Busy (something else somehow took it?)
                    // 3. Disabled don't reuse of course
                }
                should_remove = true;
                None
            });

            if should_remove {
                inner.idle.remove(key);

                #[cfg(feature = "http2")]
                {
                    inner.connecting.remove(key);
                }
            }
            entry
        };

        entry.map(|e| self.reuse(key, e))
    }


    pub(super) fn pooled(&self, key: Key, value: T) -> Pooled<T> {
        let pooled = Pooled {
            entry: Entry {
                value: value,
                is_reused: false,
                status: Rc::new(Cell::new(TimedKA::Busy)),
            },
            key: key,
            pool: Rc::downgrade(&self.inner),
        };
        if pooled.key.1 != Ver::Http1 {
            self.put(pooled.key.clone(), pooled.entry.clone());
        }
        pooled
    }

    fn is_enabled(&self) -> bool {
        self.inner.borrow().enabled
    }

    fn reuse(&self, key: &Key, mut entry: Entry<T>) -> Pooled<T> {
        debug!("reuse idle connection for {:?}", key.0);
        entry.is_reused = true;
        if key.1 == Ver::Http1 {
            entry.status.set(TimedKA::Busy);
        }
        Pooled {
            entry: entry,
            key: key.clone(),
            pool: Rc::downgrade(&self.inner),
        }
    }

    fn park(&mut self, key: Key, tx: relay::Sender<Entry<T>>, token: CancelToken) {
        trace!("park; waiting for idle connection: {:?}", key);
        self.inner.borrow_mut()
            .parked.entry(key)
            .or_insert(VecDeque::new())
            .push_back((tx, token));
    }
}

impl<T> Pool<T> {
    /// Any `FutureResponse`s that were created will have made a `Checkout`,
    /// and possibly inserted into the pool that it is waiting for an idle
    /// connection. If a user ever dropped that future, we need to clean out
    /// those parked senders.
    fn clean_parked(&mut self, key: &Key) {
        let mut inner = self.inner.borrow_mut();

        let mut remove_parked = false;
        if let Some(parked) = inner.parked.get_mut(key) {
            parked.retain(|&(ref tx, ref token)| {
                !tx.is_canceled() && !token.is_canceled()
            });
            remove_parked = parked.is_empty();
        }
        if remove_parked {
            inner.parked.remove(key);
        }
    }
}

impl<T> Clone for Pool<T> {
    fn clone(&self) -> Pool<T> {
        Pool {
            inner: self.inner.clone(),
        }
    }
}

#[derive(Clone)]
pub struct Pooled<T> {
    entry: Entry<T>,
    key: Key,
    pool: Weak<RefCell<PoolInner<T>>>,
}

impl<T> Deref for Pooled<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.entry.value
    }
}

impl<T> DerefMut for Pooled<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.entry.value
    }
}

impl<T: Clone + Ready> KeepAlive for Pooled<T> {
    fn busy(&mut self) {
        self.entry.status.set(TimedKA::Busy);
    }

    fn disable(&mut self) {
        self.entry.status.set(TimedKA::Disabled);
    }

    fn idle(&mut self) {
        let previous = self.status();
        self.entry.status.set(TimedKA::Idle(Instant::now()));
        if let KA::Idle = previous {
            trace!("Pooled::idle already idle");
            return;
        }
        self.entry.is_reused = true;
        if let Some(inner) = self.pool.upgrade() {
            let pool = Pool {
                inner: inner,
            };
            if pool.is_enabled() {
                pool.put(self.key.clone(), self.entry.clone());
            } else {
                trace!("keepalive disabled, dropping pooled ({:?})", self.key);
                self.disable();
            }
        } else {
            trace!("pool dropped, dropping pooled ({:?})", self.key);
            self.disable();
        }
    }

    fn status(&self) -> KA {
        match self.entry.status.get() {
            TimedKA::Idle(_) => KA::Idle,
            TimedKA::Busy => KA::Busy,
            TimedKA::Disabled => KA::Disabled,
        }
    }
}

impl<T> fmt::Debug for Pooled<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Pooled")
            .field("status", &self.entry.status.get())
            .field("key", &self.key)
            .finish()
    }
}

impl<T: Clone + Ready> BitAndAssign<bool> for Pooled<T> {
    fn bitand_assign(&mut self, enabled: bool) {
        if !enabled {
            self.disable();
        }
    }
}

#[derive(Clone)]
struct Entry<T> {
    value: T,
    is_reused: bool,
    status: Rc<Cell<TimedKA>>,
}

#[derive(Clone, Copy, Debug)]
enum TimedKA {
    Idle(Instant),
    Busy,
    Disabled,
}

pub struct Checkout<T> {
    cancel_token: CancelToken,
    key: Key,
    pool: Pool<T>,
    parked: Option<relay::Receiver<Entry<T>>>,
}

struct NotParked;

#[derive(Clone)]
pub struct CancelToken(Rc<Cell<bool>>);

impl<T: Clone + Ready> Checkout<T> {
    pub(super) fn key(&self) -> &Key {
        &self.key
    }

    pub(super) fn cancel_token(&self) -> &CancelToken {
        &self.cancel_token
    }

    fn poll_parked(&mut self) -> Poll<Pooled<T>, NotParked> {
        let mut drop_parked = false;
        if self.cancel_token.is_canceled() {
            drop_parked = true;
        } else if let Some(ref mut rx) = self.parked {
            match rx.poll() {
                Ok(Async::Ready(mut entry)) => {
                    if let Ok(Async::Ready(())) = entry.value.poll_ready() {
                        return Ok(Async::Ready(self.pool.reuse(&self.key, entry)));
                    }
                    drop_parked = true;
                },
                Ok(Async::NotReady) => return Ok(Async::NotReady),
                Err(_canceled) => drop_parked = true,
            }
        }
        if drop_parked {
            self.parked.take();
        }
        Err(NotParked)
    }

    fn park(&mut self) {
        if self.parked.is_none() && !self.cancel_token.is_canceled() {
            let (tx, mut rx) = relay::channel();
            let _ = rx.poll(); // park this task
            self.pool.park(self.key.clone(), tx, self.cancel_token.clone());
            self.parked = Some(rx);
        }
    }
}

impl<T: Clone + Ready> Future for Checkout<T> {
    type Item = Pooled<T>;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.poll_parked() {
            Ok(async) => return Ok(async),
            Err(_not_parked) => (),
        }

        let entry = self.pool.take(&self.key);

        if let Some(pooled) = entry {
            Ok(Async::Ready(pooled))
        } else {
            self.park();
            Ok(Async::NotReady)
        }
    }
}

impl<T> Drop for Checkout<T> {
    fn drop(&mut self) {
        self.parked.take();
        self.pool.clean_parked(&self.key);
    }
}

impl CancelToken {
    pub fn cancel(&self) {
        self.0.set(true);
    }

    fn is_canceled(&self) -> bool {
        self.0.get()
    }
}

struct Expiration(Option<Duration>);

impl Expiration {
    fn new(dur: Option<Duration>) -> Expiration {
        Expiration(dur)
    }

    fn expires(&self, instant: Instant) -> bool {
        match self.0 {
            Some(timeout) => instant.elapsed() > timeout,
            None => false,
        }
    }
}


#[cfg(test)]
mod tests {
    use std::rc::Rc;
    use std::time::Duration;
    use futures::{Async, Future, Poll};
    use futures::future;
    use proto::KeepAlive;
    use super::{Ready, Pool};
    use client::Ver;

    impl Ready for i32 {
        fn poll_ready(&mut self) -> Poll<(), ()> {
            Ok(Async::Ready(()))
        }
    }

    #[test]
    fn test_pool_checkout_smoke() {
        let pool = Pool::new(true, Some(Duration::from_secs(5)));
        let key = (Rc::new("foo".to_string()), Ver::Http1);
        let mut pooled = pool.pooled(key.clone(), 41);
        pooled.idle();

        match pool.checkout(&key.0, key.1).poll().unwrap() {
            Async::Ready(pooled) => assert_eq!(*pooled, 41),
            _ => panic!("not ready"),
        }
    }

    #[test]
    fn test_pool_checkout_returns_none_if_expired() {
        future::lazy(|| {
            let pool = Pool::new(true, Some(Duration::from_secs(1)));
            let key = (Rc::new("foo".to_string()), Ver::Http1);
            let mut pooled = pool.pooled(key.clone(), 41);
            pooled.idle();
            ::std::thread::sleep(pool.inner.borrow().timeout.unwrap());
            assert!(pool.checkout(&key.0, key.1).poll().unwrap().is_not_ready());
            ::futures::future::ok::<(), ()>(())
        }).wait().unwrap();
    }

    #[test]
    fn test_pool_removes_expired() {
        let pool = Pool::new(true, Some(Duration::from_secs(1)));
        let key = (Rc::new("foo".to_string()), Ver::Http1);

        let mut pooled1 = pool.pooled(key.clone(), 41);
        pooled1.idle();
        let mut pooled2 = pool.pooled(key.clone(), 5);
        pooled2.idle();
        let mut pooled3 = pool.pooled(key.clone(), 99);
        pooled3.idle();


        assert_eq!(pool.inner.borrow().idle.get(&key).map(|entries| entries.len()), Some(3));
        ::std::thread::sleep(pool.inner.borrow().timeout.unwrap());

        pooled1.idle();
        pooled2.idle(); // idle after sleep, not expired
        pool.checkout(&key.0, key.1).poll().unwrap();
        assert_eq!(pool.inner.borrow().idle.get(&key).map(|entries| entries.len()), Some(1));
        pool.checkout(&key.0, key.1).poll().unwrap();
        assert!(pool.inner.borrow().idle.get(&key).is_none());
    }

    #[test]
    fn test_pool_checkout_task_unparked() {
        let pool = Pool::new(true, Some(Duration::from_secs(10)));
        let key = (Rc::new("foo".to_string()), Ver::Http1);
        let pooled1 = pool.pooled(key.clone(), 41);

        let mut pooled = pooled1.clone();
        let checkout = pool.checkout(&key.0, key.1).join(future::lazy(move || {
            // the checkout future will park first,
            // and then this lazy future will be polled, which will insert
            // the pooled back into the pool
            //
            // this test makes sure that doing so will unpark the checkout
            pooled.idle();
            Ok(())
        })).map(|(entry, _)| entry);
        assert_eq!(*checkout.wait().unwrap(), *pooled1);
    }

    #[test]
    fn test_pool_checkout_drop_cleans_up_parked() {
        future::lazy(|| {
            let pool = Pool::new(true, Some(Duration::from_secs(10)));
            let key = (Rc::new("localhost:12345".to_string()), Ver::Http1);
            let _pooled1 = pool.pooled(key.clone(), 41);
            let mut checkout1 = pool.checkout(&key.0, key.1);
            let mut checkout2 = pool.checkout(&key.0, key.1);

            // first poll needed to get into Pool's parked
            checkout1.poll().unwrap();
            assert_eq!(pool.inner.borrow().parked.get(&key).unwrap().len(), 1);
            checkout2.poll().unwrap();
            assert_eq!(pool.inner.borrow().parked.get(&key).unwrap().len(), 2);

            // on drop, clean up Pool
            drop(checkout1);
            assert_eq!(pool.inner.borrow().parked.get(&key).unwrap().len(), 1);

            drop(checkout2);
            assert!(pool.inner.borrow().parked.get(&key).is_none());

            ::futures::future::ok::<(), ()>(())
        }).wait().unwrap();
    }
}
