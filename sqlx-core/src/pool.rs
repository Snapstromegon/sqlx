use crate::{
    backend::Backend,
    connection::Connection,
    error::Error,
    executor::Executor,
    params::IntoQueryParameters,
    row::{FromRow, Row},
};
use futures_channel::oneshot;
use futures_core::{future::BoxFuture, stream::BoxStream};
use futures_util::{future::{FutureExt, TryFutureExt}, stream::StreamExt};
use futures_util::future::{AbortHandle, AbortRegistration};
use std::{
    future::Future,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    sync::{
        atomic::{AtomicU32, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use async_std::future::timeout;
use async_std::sync::{channel, Receiver, Sender};
use async_std::task;

/// A pool of database connections.
pub struct Pool<DB>(Arc<SharedPool<DB>>)
where
    DB: Backend;

impl<DB> Pool<DB>
where
    DB: Backend,
{
    /// Creates a connection pool with the default configuration.
    pub async fn new(url: &str) -> crate::Result<Self> {
        Ok(Pool(Arc::new(
            SharedPool::new(url, Options::default()).await?,
        )))
    }

    /// Returns a [Builder] to configure a new connection pool.
    pub fn builder() -> Builder<DB> {
        Builder::new()
    }

    /// Retrieves a connection from the pool.
    ///
    /// Waits for at most the configured connection timeout before returning an error.
    pub async fn acquire(&self) -> crate::Result<Connection<DB>> {
        let live = self.0.acquire().await?;
        Ok(Connection::new(live, Some(Arc::clone(&self.0))))
    }

    /// Attempts to retrieve a connection from the pool if there is one available.
    ///
    /// Returns `None` if there are no idle connections available in the pool.
    /// This method will not block waiting to establish a new connection.
    pub fn try_acquire(&self) -> Option<Connection<DB>> {
        let live = self.0.try_acquire()?;
        Some(Connection::new(live, Some(Arc::clone(&self.0))))
    }

    /// Ends the use of a connection pool. Prevents any new connections
    /// and will close all active connections when they are returned to the pool.
    ///
    /// Does not resolve until all connections are closed.
    pub async fn close(&self) {
        unimplemented!()
    }

    /// Returns the number of connections currently being managed by the pool.
    pub fn size(&self) -> u32 {
        self.0.size.load(Ordering::Acquire)
    }

    /// Returns the number of idle connections.
    pub fn idle(&self) -> usize {
        self.0.pool_rx.len()
    }

    /// Returns the configured maximum pool size.
    pub fn max_size(&self) -> u32 {
        self.0.options.max_size
    }

    /// Returns the maximum time spent acquiring a new connection before an error is returned.
    pub fn connect_timeout(&self) -> Duration {
        self.0.options.connect_timeout
    }

    /// Returns the configured mimimum idle connection count.
    pub fn min_idle(&self) -> Option<u32> {
        self.0.options.min_idle
    }

    /// Returns the configured maximum connection lifetime.
    pub fn max_lifetime(&self) -> Option<Duration> {
        self.0.options.max_lifetime
    }

    /// Returns the configured idle connection timeout.
    pub fn idle_timeout(&self) -> Option<Duration> {
        self.0.options.idle_timeout
    }
}

/// Returns a new [Pool] tied to the same shared connection pool.
impl<DB> Clone for Pool<DB>
where
    DB: Backend,
{
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

#[derive(Default)]
pub struct Builder<DB>
where
    DB: Backend,
{
    phantom: PhantomData<DB>,
    options: Options,
}

impl<DB> Builder<DB>
where
    DB: Backend,
{
    fn new() -> Self {
        Self {
            phantom: PhantomData,
            options: Options::default(),
        }
    }

    pub fn max_size(mut self, max_size: u32) -> Self {
        self.options.max_size = max_size;
        self
    }

    pub fn connect_timeout(mut self, connect_timeout: Duration) -> Self {
        self.options.connect_timeout = connect_timeout;
        self
    }

    pub fn min_idle(mut self, min_idle: impl Into<Option<u32>>) -> Self {
        self.options.min_idle = min_idle.into();
        self
    }

    pub fn max_lifetime(mut self, max_lifetime: impl Into<Option<Duration>>) -> Self {
        self.options.max_lifetime = max_lifetime.into();
        self
    }

    pub fn idle_timeout(mut self, idle_timeout: impl Into<Option<Duration>>) -> Self {
        self.options.idle_timeout = idle_timeout.into();
        self
    }

    pub async fn build(self, url: &str) -> crate::Result<Pool<DB>> {
        Ok(Pool(Arc::new(SharedPool::new(url, self.options).await?)))
    }
}

struct Options {
    max_size: u32,
    connect_timeout: Duration,
    min_idle: Option<u32>,
    max_lifetime: Option<Duration>,
    idle_timeout: Option<Duration>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            max_size: 10,
            min_idle: None,
            connect_timeout: Duration::from_secs(30),
            max_lifetime: None,
            idle_timeout: None,
        }
    }
}

pub(crate) struct SharedPool<DB>
where
    DB: Backend,
{
    url: String,
    pool_rx: Receiver<Idle<DB>>,
    pool_tx: Sender<Idle<DB>>,
    size: AtomicU32,
    options: Options,
}

impl<DB> SharedPool<DB>
where
    DB: Backend,
{
    async fn new(url: &str, options: Options) -> crate::Result<Self> {
        // TODO: Establish [min_idle] connections

        let (pool_tx, pool_rx) = channel(options.max_size as usize);

        Ok(Self {
            url: url.to_owned(),
            pool_rx,
            pool_tx,
            size: AtomicU32::new(0),
            options,
        })
    }

    #[inline]
    fn try_acquire(&self) -> Option<Live<DB>> {
        Some(self.pool_rx.recv().now_or_never()??.live(&self.pool_tx))
    }

    async fn acquire(&self) -> crate::Result<Live<DB>> {
        let start = Instant::now();
        let deadline = start + self.options.connect_timeout;

        if let Some(live) = self.try_acquire() {
            return Ok(live);
        }

        loop {
            let size = self.size.load(Ordering::Acquire);

            if size >= self.options.max_size {
                // Too many open connections
                // Wait until one is available

                // get the time between the deadline and now and use that as our timeout
                let max_wait = deadline.checked_duration_since(Instant::now())
                    .ok_or(Error::TimedOut)?;

                // don't sleep forever
                let idle = match timeout(max_wait, self.pool_rx.recv()).await {
                    Ok(Some(idle)) => idle,
                    Ok(None) => panic!("this isn't possible, we own a `pool_tx`"),
                    // try our acquire logic again
                    Err(_) => continue,
                };

                // check if idle connection was within max lifetime (or not set)
                if self.options.max_lifetime.map_or(true, |max| idle.raw.created.elapsed() < max)
                    // and if connection wasn't idle too long (or not set)
                    && self.options.idle_timeout.map_or(true, |timeout| idle.since.elapsed() < timeout)
                {
                    match idle.raw.inner.ping().await {
                        Ok(_) => return Ok(idle.revive(&self.pool_tx)),
                        // an error here means the other end has hung up or we lost connectivity
                        // either way we're fine to just discard the connection
                        // the error itself here isn't necessarily unexpected so WARN is too strong
                        Err(e) => log::info!("ping on idle connection returned error: {}", e),
                    }
                } else {
                    // close the connection but don't really care about the result
                    let _ = idle.raw.inner.close().await;
                }

                // either case, make sure the idle connection is gone explicitly before we open one
                drop(idle);

                // while we're still at max size, acquire a new connection
                return self.new_conn(deadline).await
            }

            if self.size.compare_and_swap(size, size + 1, Ordering::AcqRel) == size {
                // Open a new connection and return directly
                return self.new_conn(deadline).await
            }
        }
    }

    async fn new_conn(&self, deadline: Instant) -> crate::Result<Live<DB>> {
        while Instant::now() < deadline {
            // result here is `Result<Result<DB, Error>, TimeoutError>`
            match timeout(deadline - Instant::now(), DB::open(&self.url)).await {
                Ok(Ok(raw)) => return Ok(Live::pooled(raw, &self.pool_tx)),
                // error while connecting, this should definitely be logged
                Ok(Err(e)) => log::warn!("error establishing a connection: {}", e),
                // timed out
                Err(_) => break,
            }
        }

        self.size.fetch_sub(1, Ordering::AcqRel);
        Err(Error::TimedOut)
    }
}

impl<DB> Executor for Pool<DB>
where
    DB: Backend,
{
    type Backend = DB;

    fn execute<'c, 'q: 'c, A: 'c>(
        &'c mut self,
        query: &'q str,
        params: A,
    ) -> BoxFuture<'c, Result<u64, Error>>
    where
        A: IntoQueryParameters<Self::Backend> + Send,
    {
        Box::pin(async move { <&Pool<DB> as Executor>::execute(&mut &*self, query, params).await })
    }

    fn fetch<'c, 'q: 'c, T: 'c, A: 'c>(
        &'c mut self,
        query: &'q str,
        params: A,
    ) -> BoxStream<'c, Result<T, Error>>
    where
        A: IntoQueryParameters<Self::Backend> + Send,
        T: FromRow<Self::Backend> + Send + Unpin,
    {
        Box::pin(async_stream::try_stream! {
            let mut self_ = &*self;
            let mut s = <&Pool<DB> as Executor>::fetch(&mut self_, query, params);

            while let Some(row) = s.next().await.transpose()? {
                yield row;
            }

            drop(s);
        })
    }

    fn fetch_optional<'c, 'q: 'c, T: 'c, A: 'c>(
        &'c mut self,
        query: &'q str,
        params: A,
    ) -> BoxFuture<'c, Result<Option<T>, Error>>
    where
        A: IntoQueryParameters<Self::Backend> + Send,
        T: FromRow<Self::Backend> + Send,
    {
        Box::pin(async move {
            <&Pool<DB> as Executor>::fetch_optional(&mut &*self, query, params).await
        })
    }
}

impl<DB> Executor for &'_ Pool<DB>
where
    DB: Backend,
{
    type Backend = DB;

    fn execute<'c, 'q: 'c, A: 'c>(
        &'c mut self,
        query: &'q str,
        params: A,
    ) -> BoxFuture<'c, Result<u64, Error>>
    where
        A: IntoQueryParameters<Self::Backend> + Send,
    {
        Box::pin(async move {
            let mut live = self.0.acquire().await?;
            let result = live.execute(query, params.into_params()).await;
            result
        })
    }

    fn fetch<'c, 'q: 'c, T: 'c, A: 'c>(
        &'c mut self,
        query: &'q str,
        params: A,
    ) -> BoxStream<'c, Result<T, Error>>
    where
        A: IntoQueryParameters<Self::Backend> + Send,
        T: FromRow<Self::Backend> + Send + Unpin,
    {
        Box::pin(async_stream::try_stream! {
            let mut live = self.0.acquire().await?;
            let mut s = live.fetch(query, params.into_params());

            while let Some(row) = s.next().await.transpose()? {
                yield T::from_row(Row(row));
            }
        })
    }

    fn fetch_optional<'c, 'q: 'c, T: 'c, A: 'c>(
        &'c mut self,
        query: &'q str,
        params: A,
    ) -> BoxFuture<'c, Result<Option<T>, Error>>
    where
        A: IntoQueryParameters<Self::Backend> + Send,
        T: FromRow<Self::Backend> + Send,
    {
        Box::pin(async move {
            Ok(self
                .0
                .acquire()
                .await?
                .fetch_optional(query, params.into_params())
                .await?
                .map(Row)
                .map(T::from_row))
        })
    }
}

struct Raw<DB> {
    pub(crate) inner: DB,
    pub(crate) created: Instant,
}

struct Idle<DB>
where
    DB: Backend,
{
    raw: Raw<DB>,
    #[allow(unused)]
    since: Instant,
}

impl<DB: Backend> Idle<DB> {
    fn revive(self, pool_tx: &Sender<Idle<DB>>) -> Live<DB> {
        Live {
            raw: Some(self.raw),
            pool_tx: Some(pool_tx.clone()),
        }
    }
}

pub(crate) struct Live<DB>
where
    DB: Backend,
{
    raw: Option<Raw<DB>>,
    pool_tx: Option<Sender<Idle<DB>>>,
}

impl<DB: Backend> Live<DB> {
    pub fn unpooled(raw: DB) -> Self {
        Live {
            raw: Some(Raw {
                inner: raw,
                created: Instant::now(),
            }),
            pool_tx: None,
        }
    }

    fn pooled(raw: DB, pool_tx: &Sender<Idle<DB>>) -> Self {
        Live {
            raw: Some(Raw {
                inner: raw,
                created: Instant::now(),
            }),
            pool_tx: Some(pool_tx.clone()),
        }
    }

    pub fn release(mut self) {
        self.release_mut()
    }

    fn release_mut(&mut self) {
        // `.release_mut()` will be called twice if `.release()` is called
        if let (Some(raw), Some(pool_tx)) = (self.raw.take(), self.pool_tx.as_ref()) {
            pool_tx
                .send(Idle {
                    raw,
                    since: Instant::now(),
                })
                .now_or_never()
                .expect("(bug) connection released into a full pool")
        }
    }
}

const DEREF_ERR: &str = "(bug) connection already released to pool";

impl<DB: Backend> Deref for Live<DB> {
    type Target = DB;

    fn deref(&self) -> &DB {
        &self.raw.as_ref().expect(DEREF_ERR).inner
    }
}

impl<DB: Backend> DerefMut for Live<DB> {
    fn deref_mut(&mut self) -> &mut DB {
        &mut self.raw.as_mut().expect(DEREF_ERR).inner
    }
}

impl<DB: Backend> Drop for Live<DB> {
    fn drop(&mut self) {
        self.release_mut()
    }
}
