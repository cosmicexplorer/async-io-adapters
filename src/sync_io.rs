use crate::ring::{
  futurized::{RingFuturized, WritePermitFuturized},
  ring_buffer::{Ring, TruncateLength},
};

use async_executors::iface::{SpawnBlocking, SpawnHandle, SpawnHandleExt};
use pin_project_lite::pin_project;
use trait_set::trait_set;

use std::{
  future::Future,
  io, mem,
  pin::Pin,
  sync::Arc,
  task::{ready, Context, Poll},
};


#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum PrefetchBehavior {
  AsRequested,
  AsManyAsPossible,
}

trait_set! {
  pub trait Exe = SpawnHandle<()> + SpawnBlocking<()> + Send + Sync;
}

pin_project! {
  ///```
  /// # fn main() -> std::io::Result<()> { tokio_test::block_on(async {
  /// use async_io_adapters::{ring::Ring, sync_io::*};
  /// use std::{io::Cursor, sync::Arc};
  /// use async_executors::exec::AsyncGlobal;
  /// use tokio::io::AsyncReadExt;
  ///
  /// let mut stream = Cursor::new(Vec::new());
  /// {
  ///   use std::io::prelude::*;
  ///   stream.write_all(b"hello\n")?;
  ///   stream.rewind()?;
  /// }
  ///
  /// let exec = Arc::new(AsyncGlobal::new());
  /// let prefetch = PrefetchBehavior::AsRequested;
  /// let ring = Ring::with_capacity(20);
  /// let mut f = AsyncIoAdapter::new(stream, ring, exec, prefetch);
  /// let mut buf = String::new();
  /// f.read_to_string(&mut buf).await?;
  /// let _stream = f.into_inner();
  /// assert_eq!(buf.len(), "hello\n".len());
  /// assert_eq!(&buf, "hello\n");
  /// # Ok(())
  /// # })}
  ///```
  pub struct AsyncIoAdapter<S> {
    inner: Arc<async_lock::Mutex<Option<S>>>,
    tx: Arc<parking_lot::Mutex<Option<oneshot::Sender<io::Result<()>>>>>,
    #[pin]
    rx: oneshot::Receiver<io::Result<()>>,
    ring: RingFuturized,
    executor: Arc<dyn Exe>,
    prefetch: PrefetchBehavior,
  }
}

impl<S> AsyncIoAdapter<S> {
  pub fn new(inner: S, ring: Ring, executor: Arc<dyn Exe>, prefetch: PrefetchBehavior) -> Self {
    let (tx, rx) = oneshot::channel::<io::Result<()>>();
    Self {
      inner: Arc::new(async_lock::Mutex::new(Some(inner))),
      tx: Arc::new(parking_lot::Mutex::new(Some(tx))),
      rx,
      ring: RingFuturized::wrap_ring(ring),
      executor,
      prefetch,
    }
  }

  pub fn into_inner(self) -> S {
    self
      .inner
      .try_lock_arc()
      .expect("there should be no further handles to this mutex")
      .take()
      .unwrap()
  }

  fn get_pin_rx(self: Pin<&mut Self>) -> Pin<&mut oneshot::Receiver<io::Result<()>>> {
    self.project().rx
  }

  fn get_mut_ring(self: Pin<&mut Self>) -> &mut RingFuturized { self.project().ring }
}

impl<S: io::Read> AsyncIoAdapter<S> {
  fn do_write(
    mut write_lease: mem::ManuallyDrop<WritePermitFuturized>,
    mut inner: async_lock::MutexGuardArc<Option<S>>,
    tx: Arc<parking_lot::Mutex<Option<oneshot::Sender<io::Result<()>>>>>,
  ) {
    debug_assert!(!write_lease.is_empty());
    dbg!(&write_lease);
    match inner.as_mut().unwrap().read(&mut write_lease) {
      Ok(n) => {
        eprintln!("here(n = {})", n);
        write_lease.truncate(n);
        /* If we received 0 after providing a non-empty output buf, assume we are
         * OVER! */
        if n == 0 {
          if let Some(tx) = tx.lock().take() {
            tx.send(Ok(()))
              .expect("receiver should not have been dropped yet!");
          }
        }
      },
      Err(e) => {
        eprintln!("err(e = {})", &e);
        /* If any error occurs, assume no bytes were written, as per std::io::Read
         * docs. */
        write_lease.truncate(0);
        if e.kind() != io::ErrorKind::Interrupted {
          /* Interrupted is the only retryable error according to the docs. */
          if let Some(tx) = tx.lock().take() {
            tx.send(Err(e))
              .expect("receiver should not have been dropped yet!");
          }
        }
      },
    }
    unsafe {
      mem::ManuallyDrop::drop(&mut write_lease);
    }
  }
}

#[cfg(feature = "futures-io")]
impl<S: io::Read+Send+'static> futures_util::AsyncRead for AsyncIoAdapter<S> {
  fn poll_read(
    mut self: Pin<&mut Self>,
    cx: &mut Context<'_>,
    buf: &mut [u8],
  ) -> Poll<io::Result<usize>> {
    debug_assert!(buf.len() > 0);

    if let Poll::Ready(read_data) = self.as_mut().get_mut_ring().poll_read(cx, buf.len()) {
      debug_assert!(!read_data.is_empty());
      debug_assert!(read_data.len() <= buf.len());
      buf[..read_data.len()].copy_from_slice(&**read_data);
      return Poll::Ready(Ok(read_data.len()));
    }

    if let Poll::Ready(result) = self.as_mut().get_pin_rx().poll(cx) {
      return Poll::Ready(
        result
          .expect("sender should not have been dropped without sending!")
          .map(|()| 0),
      );
    }

    let requested_length: usize = match self.prefetch {
      PrefetchBehavior::AsRequested => buf.len(),
      PrefetchBehavior::AsManyAsPossible => self.ring.capacity(),
    };
    let write_data = ready!(self
      .as_mut()
      .get_mut_ring()
      .poll_write(cx, requested_length));
    let write_data: mem::ManuallyDrop<WritePermitFuturized<'static>> =
      mem::ManuallyDrop::new(unsafe { mem::transmute(write_data) });

    let tx = self.tx.clone();
    match self.inner.try_lock_arc() {
      Some(inner) => {
        self.executor.spawn_blocking_dyn(Box::new(move || {
          Self::do_write(write_data, inner, tx);
        }));
        Poll::Pending
      },
      None => {
        let inner = self.inner.clone();
        let exe = self.executor.clone();
        self
          .executor
          .spawn_handle(async move {
            let inner = inner.lock_arc().await;
            exe.spawn_blocking_dyn(Box::new(move || {
              Self::do_write(write_data, inner, tx);
            }));
          })
          .unwrap()
          .detach();
        Poll::Pending
      },
    }
  }
}

#[cfg(feature = "tokio-io")]
impl<S: io::Read+Send+'static> tokio::io::AsyncRead for AsyncIoAdapter<S> {
  fn poll_read(
    mut self: Pin<&mut Self>,
    cx: &mut Context<'_>,
    buf: &mut tokio::io::ReadBuf<'_>,
  ) -> Poll<io::Result<()>> {
    debug_assert!(buf.remaining() > 0);

    if let Poll::Ready(read_data) = self.as_mut().get_mut_ring().poll_read(cx, buf.remaining()) {
      dbg!(&read_data);
      dbg!(read_data.len());
      panic!("ok");
      debug_assert!(!read_data.is_empty());
      debug_assert!(read_data.len() <= buf.remaining());
      buf.put_slice(&**read_data);
      return Poll::Ready(Ok(()));
    }

    if let Poll::Ready(result) = self.as_mut().get_pin_rx().poll(cx) {
      return Poll::Ready(result.expect("sender should not have been dropped without sending!"));
    }

    let requested_length: usize = match self.prefetch {
      PrefetchBehavior::AsRequested => buf.remaining(),
      PrefetchBehavior::AsManyAsPossible => self.ring.capacity(),
    };
    let write_data = ready!(self
      .as_mut()
      .get_mut_ring()
      .poll_write(cx, requested_length));
    dbg!(&write_data);
    let write_data: mem::ManuallyDrop<WritePermitFuturized<'static>> =
      mem::ManuallyDrop::new(unsafe { mem::transmute(write_data) });

    let tx = self.tx.clone();
    match self.inner.try_lock_arc() {
      Some(inner) => {
        self.executor.spawn_blocking_dyn(Box::new(move || {
          dbg!(&write_data);
          Self::do_write(write_data, inner, tx);
        }));
        Poll::Pending
      },
      None => {
        let inner = self.inner.clone();
        let exe = self.executor.clone();
        self
          .executor
          .spawn_handle(async move {
            let inner = inner.lock_arc().await;
            exe.spawn_blocking_dyn(Box::new(move || {
              dbg!(&write_data);
              Self::do_write(write_data, inner, tx);
            }));
          })
          .unwrap()
          .detach();
        Poll::Pending
      },
    }
  }
}
