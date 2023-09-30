#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum LeaseBehavior {
  AllowSpuriousFailures,
  NoSpuriousFailures,
}

#[derive(Debug, Copy, Clone)]
pub enum Lease<Permit> {
  NoSpace,
  PossiblyTaken,
  Taken(Permit),
}

impl<Permit> Lease<Permit> {
  #[inline]
  pub fn option(self) -> Option<Permit> {
    match self {
      Self::NoSpace => None,
      Self::PossiblyTaken => None,
      Self::Taken(permit) => Some(permit),
    }
  }
}

#[repr(u8)]
#[derive(
  Debug, Copy, Clone, PartialEq, Eq, Default, num_enum::TryFromPrimitive, num_enum::IntoPrimitive,
)]
pub enum PermitState {
  #[default]
  Unleashed = 0,
  TakenOut = 1,
}

pub mod ring {
  use super::{Lease, LeaseBehavior, PermitState};

  use std::{
    cmp, ops, slice,
    sync::atomic::{AtomicU8, AtomicUsize, Ordering},
  };

  #[derive(Debug)]
  struct PermitFlag {
    state: AtomicU8,
  }

  impl Default for PermitFlag {
    fn default() -> Self {
      Self {
        state: AtomicU8::new(PermitState::Unleashed.into()),
      }
    }
  }

  impl PermitFlag {
    pub fn reset(&mut self) { *self.state.get_mut() = PermitState::Unleashed.into(); }

    #[inline]
    pub fn load(&self, ordering: Ordering) -> PermitState {
      self.state.load(ordering).try_into().unwrap()
    }

    #[inline]
    fn store(&self, val: PermitState, ordering: Ordering) { self.state.store(val.into(), ordering) }

    #[inline]
    pub fn release(&self) { self.store(PermitState::Unleashed, Ordering::Release) }

    #[inline]
    pub fn try_lease(&self, behavior: LeaseBehavior) -> bool {
      let current = PermitState::Unleashed;
      let new = PermitState::TakenOut;
      match behavior {
        LeaseBehavior::AllowSpuriousFailures => self.state.compare_exchange_weak(
          current.into(),
          new.into(),
          Ordering::AcqRel,
          Ordering::Relaxed,
        ),
        LeaseBehavior::NoSpuriousFailures => self.state.compare_exchange(
          current.into(),
          new.into(),
          Ordering::AcqRel,
          Ordering::Relaxed,
        ),
      }
      .is_ok()
    }
  }

  #[derive(Debug)]
  struct Half {
    flag: PermitFlag,
    head: AtomicUsize,
    remaining_inline: AtomicUsize,
    capacity: usize,
  }

  impl AsRef<PermitFlag> for Half {
    #[inline]
    fn as_ref(&self) -> &PermitFlag { &self.flag }
  }

  impl AsMut<PermitFlag> for Half {
    #[inline]
    fn as_mut(&mut self) -> &mut PermitFlag { &mut self.flag }
  }

  impl Half {
    pub fn read_new(capacity: usize) -> Self {
      Self {
        flag: PermitFlag::default(),
        head: AtomicUsize::new(0),
        remaining_inline: AtomicUsize::new(0),
        capacity,
      }
    }

    pub fn read_reset(&mut self) {
      self.as_mut().reset();
      *self.remaining_inline.get_mut() = 0;
      *self.head.get_mut() = 0;
    }

    pub fn write_new(capacity: usize) -> Self {
      Self {
        flag: PermitFlag::default(),
        head: AtomicUsize::new(0),
        remaining_inline: AtomicUsize::new(capacity),
        capacity,
      }
    }

    pub fn write_reset(&mut self) {
      self.as_mut().reset();
      *self.remaining_inline.get_mut() = self.capacity;
      *self.head.get_mut() = 0;
    }

    #[inline]
    pub fn optimistic_is_empty(&self) -> bool { self.remaining_inline.load(Ordering::Relaxed) == 0 }

    #[inline]
    pub fn remaining_inline(&self) -> usize { self.remaining_inline.load(Ordering::Acquire) }

    #[inline]
    pub fn fetch_sub_inline(&self, len: usize) {
      debug_assert!(len > 0);
      let prev_inline = self.remaining_inline.fetch_sub(len, Ordering::Release);
      debug_assert!(prev_inline >= len);
    }

    #[inline]
    pub fn fetch_add_inline(&self, len: usize) {
      debug_assert!(len > 0);
      let prev_inline = self.remaining_inline.fetch_add(len, Ordering::Release);
      debug_assert!(prev_inline + len < self.capacity);
    }

    #[inline]
    pub fn head(&self) -> usize { self.head.load(Ordering::Acquire) }

    #[inline]
    fn store_head(&self, at: usize) {
      debug_assert!(at < self.capacity);
      self.head.store(at, Ordering::Release)
    }

    pub fn reclaim(&self, other: &Self, len: usize) {
      debug_assert!(len > 0);

      let my_new_head = len + self.head();
      let other_head = other.head();

      if my_new_head == self.capacity {
        debug_assert_eq!(0, self.remaining_inline());
        self.fetch_add_inline(other_head);
        self.store_head(0);
      } else {
        self.store_head(my_new_head);
      }

      if my_new_head > other_head {
        other.fetch_add_inline(len);
      }
    }
  }

  ///```
  /// use async_io_adapters::ring::*;
  /// use std::sync::Arc;
  ///
  /// let ring = Arc::new(Ring::with_capacity(10));
  ///
  /// assert!(matches![ring.read_lease_strong(1), Lease::NoSpace]);
  /// {
  ///   let mut write_lease = ring.write_lease_strong(5).option().unwrap();
  ///   write_lease.copy_from_slice(b"world");
  /// }
  /// {
  ///   let read_lease = ring.read_lease_strong(5).option().unwrap();
  ///   assert_eq!(std::str::from_utf8(&*read_lease).unwrap(), "world");
  /// }
  /// {
  ///   let mut write_lease = ring.write_lease_strong(6).option().unwrap();
  ///   assert_eq!(5, write_lease.len());
  ///   write_lease.copy_from_slice(b"hello");
  /// }
  /// {
  ///   let read_lease = ring.read_lease_strong(4).option().unwrap();
  ///   assert_eq!(std::str::from_utf8(&*read_lease).unwrap(), "hell");
  /// }
  /// {
  ///   let mut write_lease = ring.write_lease_strong(2).option().unwrap();
  ///   write_lease.copy_from_slice(b"k!");
  /// }
  /// let mut buf = Vec::new();
  /// {
  ///   let read_lease = ring.read_lease_strong(3).option().unwrap();
  ///   assert_eq!(1, read_lease.len());
  ///   buf.extend_from_slice(&read_lease);
  /// }
  /// {
  ///   let read_lease = ring.read_lease_strong(3).option().unwrap();
  ///   assert_eq!(2, read_lease.len());
  ///   buf.extend_from_slice(&read_lease);
  /// }
  /// assert_eq!(std::str::from_utf8(&buf).unwrap(), "ok!");
  /// assert!(matches![ring.read_lease_strong(1), Lease::NoSpace]);
  /// ```
  #[derive(Debug)]
  pub struct Ring {
    buf: Box<[u8]>,
    write: Half,
    read: Half,
  }

  impl Ring {
    pub fn clear(&mut self) {
      self.buf.fill(0);
      self.write.write_reset();
      self.read.read_reset();
    }

    pub fn with_capacity(capacity: usize) -> Self {
      assert!(capacity > 0);
      Self {
        buf: vec![0u8; capacity].into_boxed_slice(),
        write: Half::write_new(capacity),
        read: Half::read_new(capacity),
      }
    }

    #[inline]
    pub fn capacity(&self) -> usize { self.buf.len() }

    pub(crate) fn return_write_lease(&self, permit: &WritePermit<'_>) {
      debug_assert!(self.write.as_ref().load(Ordering::Relaxed) == PermitState::TakenOut);

      let truncated_length = permit.truncated_length();
      if truncated_length > 0 {
        self.write.fetch_add_inline(truncated_length);
      }

      let len = permit.len();

      if len > 0 {
        self.write.reclaim(&self.read, len);
      }

      self.write.as_ref().release();
    }

    pub(crate) fn request_write_lease(
      &self,
      requested_length: usize,
      behavior: LeaseBehavior,
    ) -> Lease<WritePermit<'_>> {
      assert!(requested_length > 0);
      if self.write.optimistic_is_empty() {
        return Lease::NoSpace;
      }

      if !self.write.as_ref().try_lease(behavior) {
        return Lease::PossiblyTaken;
      }

      let remaining_inline_write = self.write.remaining_inline();
      if remaining_inline_write == 0 {
        self.write.as_ref().release();
        return Lease::NoSpace;
      }

      let limited_length = cmp::min(remaining_inline_write, requested_length);
      debug_assert!(limited_length > 0);
      self.write.fetch_sub_inline(limited_length);

      let prev_write_head = self.write.head();

      let buf: &mut [u8] = unsafe {
        let buf: *const u8 = self.buf.as_ptr();
        let start = buf.add(prev_write_head) as *mut u8;
        slice::from_raw_parts_mut(start, limited_length)
      };
      Lease::Taken(WritePermit::view(buf, self))
    }

    pub fn write_lease_strong(&self, requested_length: usize) -> Lease<WritePermit<'_>> {
      self.request_write_lease(requested_length, LeaseBehavior::NoSpuriousFailures)
    }

    pub fn write_lease_weak(&self, requested_length: usize) -> Lease<WritePermit<'_>> {
      self.request_write_lease(requested_length, LeaseBehavior::AllowSpuriousFailures)
    }

    pub(crate) fn return_read_lease(&self, permit: &ReadPermit<'_>) {
      debug_assert!(self.read.as_ref().load(Ordering::Relaxed) == PermitState::TakenOut);

      let truncated_length = permit.truncated_length();
      if truncated_length > 0 {
        self.read.fetch_add_inline(truncated_length);
      }

      let len = permit.len();

      if len > 0 {
        self.read.reclaim(&self.write, len);
      }

      self.read.as_ref().release();
    }

    pub(crate) fn request_read_lease(
      &self,
      requested_length: usize,
      behavior: LeaseBehavior,
    ) -> Lease<ReadPermit<'_>> {
      assert!(requested_length > 0);
      if self.read.optimistic_is_empty() {
        return Lease::NoSpace;
      }

      if !self.read.as_ref().try_lease(behavior) {
        return Lease::PossiblyTaken;
      }

      let remaining_inline_read = self.read.remaining_inline();
      if remaining_inline_read == 0 {
        self.read.as_ref().release();
        return Lease::NoSpace;
      }

      let limited_length = cmp::min(remaining_inline_read, requested_length);
      debug_assert!(limited_length > 0);
      self.read.fetch_sub_inline(limited_length);

      let prev_read_head = self.read.head();

      let buf: &[u8] = unsafe {
        let buf: *const u8 = self.buf.as_ptr();
        let start = buf.add(prev_read_head);
        slice::from_raw_parts(start, limited_length)
      };
      Lease::Taken(ReadPermit::view(buf, self))
    }

    pub fn read_lease_strong(&self, requested_length: usize) -> Lease<ReadPermit<'_>> {
      self.request_read_lease(requested_length, LeaseBehavior::NoSpuriousFailures)
    }

    pub fn read_lease_weak(&self, requested_length: usize) -> Lease<ReadPermit<'_>> {
      self.request_read_lease(requested_length, LeaseBehavior::AllowSpuriousFailures)
    }
  }

  ///```
  /// use async_io_adapters::ring::*;
  /// use std::sync::Arc;
  ///
  /// let msg = "hello world";
  /// let ring = Arc::new(Ring::with_capacity(30));
  ///
  /// let mut buf = Vec::new();
  /// {
  ///   let mut write_lease = ring.write_lease_strong(5).option().unwrap();
  ///   write_lease.copy_from_slice(&msg.as_bytes()[..5]);
  ///   write_lease.truncate(4);
  ///   assert_eq!(4, write_lease.len());
  /// }
  /// {
  ///   let mut read_lease = ring.read_lease_strong(5).option().unwrap();
  ///   assert_eq!(4, read_lease.len());
  ///   buf.extend_from_slice(read_lease.truncate(1));
  ///   assert_eq!(1, buf.len());
  ///   assert_eq!(1, read_lease.len());
  /// }
  /// {
  ///   let mut write_lease = ring.write_lease_strong(msg.len() - 4).option().unwrap();
  ///   write_lease.copy_from_slice(&msg.as_bytes()[4..]);
  /// }
  /// {
  ///   let read_lease = ring.read_lease_strong(msg.len() - 1).option().unwrap();
  ///   assert_eq!(read_lease.len(), msg.len() - 1);
  ///   buf.extend_from_slice(&read_lease);
  /// }
  /// assert_eq!(msg, std::str::from_utf8(&buf).unwrap());
  /// ```
  pub trait TruncateLength {
    fn truncated_length(&self) -> usize;
    fn truncate(&mut self, len: usize) -> &mut Self;
  }

  #[derive(Debug)]
  pub struct ReadPermit<'a> {
    view: &'a [u8],
    parent: &'a Ring,
    original_length: usize,
  }

  impl<'a> ReadPermit<'a> {
    pub(crate) fn view(view: &'a [u8], parent: &'a Ring) -> Self {
      let original_length = view.len();
      Self {
        view,
        parent,
        original_length,
      }
    }
  }

  impl<'a> TruncateLength for ReadPermit<'a> {
    #[inline]
    fn truncated_length(&self) -> usize { self.original_length - self.len() }

    #[inline]
    fn truncate(&mut self, len: usize) -> &mut Self {
      assert!(len <= self.len());
      self.view = &self.view[..len];
      self
    }
  }

  impl<'a> ops::Drop for ReadPermit<'a> {
    fn drop(&mut self) { self.parent.return_read_lease(self); }
  }

  impl<'a> AsRef<[u8]> for ReadPermit<'a> {
    #[inline]
    fn as_ref(&self) -> &[u8] { &self.view }
  }

  impl<'a> ops::Deref for ReadPermit<'a> {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] { &self.view }
  }

  #[derive(Debug)]
  pub struct WritePermit<'a> {
    view: &'a mut [u8],
    parent: &'a Ring,
    original_length: usize,
  }

  impl<'a> WritePermit<'a> {
    pub(crate) fn view(view: &'a mut [u8], parent: &'a Ring) -> Self {
      let original_length = view.len();
      Self {
        view,
        parent,
        original_length,
      }
    }
  }

  impl<'a> TruncateLength for WritePermit<'a> {
    #[inline]
    fn truncated_length(&self) -> usize { self.original_length - self.len() }

    #[inline]
    fn truncate(&mut self, len: usize) -> &mut Self {
      assert!(len <= self.len());
      self.view = unsafe { slice::from_raw_parts_mut(self.view.as_ptr() as *mut u8, len) };
      self
    }
  }

  impl<'a> ops::Drop for WritePermit<'a> {
    fn drop(&mut self) { self.parent.return_write_lease(self); }
  }

  impl<'a> AsRef<[u8]> for WritePermit<'a> {
    #[inline]
    fn as_ref(&self) -> &[u8] { &self.view }
  }

  impl<'a> AsMut<[u8]> for WritePermit<'a> {
    #[inline]
    fn as_mut(&mut self) -> &mut [u8] { &mut self.view }
  }

  impl<'a> ops::Deref for WritePermit<'a> {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] { &self.view }
  }

  impl<'a> ops::DerefMut for WritePermit<'a> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [u8] { &mut self.view }
  }
}
pub use ring::{ReadPermit, Ring, TruncateLength, WritePermit};

/* pub mod push { */
/* use super::PermitState; */

/* use std::{ */
/* cell, mem, */
/* sync::atomic::{AtomicU8, Ordering}, */
/* }; */

/* pub struct Pusher<T> { */
/* elements: cell::UnsafeCell<Vec<T>>, */
/* state: AtomicU8, */
/* } */

/* impl<T> Pusher<T> { */
/* pub fn new() -> Self { */
/* Self { */
/* elements: cell::UnsafeCell::new(Vec::new()), */
/* state: AtomicU8::new(PermitState::Unleashed.into()), */
/* } */
/* } */

/* fn within_lock<O, F: FnOnce(&mut Vec<T>) -> O>(&self, f: F) -> O { */
/* while let Err(_) = self.state.compare_exchange_weak( */
/* PermitState::Unleashed.into(), */
/* PermitState::TakenOut.into(), */
/* Ordering::AcqRel, */
/* Ordering::Relaxed, */
/* ) {} */

/* let v: &mut Vec<T> = unsafe { &mut *self.elements.get() }; */
/* let ret = f(v); */

/* self */
/* .state */
/* .store(PermitState::Unleashed.into(), Ordering::Release); */

/* ret */
/* } */

/* pub fn push(&self, x: T) { self.within_lock(|v| v.push(x)) } */

/* pub fn extract(&self) -> Vec<T> { self.within_lock(|v| mem::take(v)) } */

/* pub fn take_owned(&mut self) -> Vec<T> { */
/* mem::replace(&mut self.elements,
 * cell::UnsafeCell::new(Vec::new())).into_inner() */
/* } */
/* } */

/* unsafe impl<T: Send> Sync for Pusher<T> {} */
/* } */

/* pub mod futurized { */
/* use super::{ */
/* push::Pusher, */
/* ring::{ReadPermit, Ring, WritePermit}, */
/* Lease, */
/* }; */

/* use once_cell::sync::Lazy; */
/* use parking_lot::Mutex; */

/* use std::{ */
/* collections::VecDeque, */
/* mem, ops, */
/* task::{Context, Poll, Waker}, */
/* }; */

/* static RING_BUF_FREE_LIST: Lazy<Mutex<VecDeque<Ring>>> = */
/* Lazy::new(|| Mutex::new(VecDeque::new())); */

/* fn get_or_create_ring<F: FnOnce() -> Ring>(f: F) -> Ring { */
/* RING_BUF_FREE_LIST.lock().pop_front().unwrap_or_else(f) */
/* } */

/* fn return_ring(mut ring: Ring) { */
/* ring.clear(); */
/* RING_BUF_FREE_LIST.lock().push_back(ring); */
/* } */

/* ///``` */
/* /// # fn main() { tokio_test::block_on(async { */
/* /// use zip::channels::{*, futurized::*}; */
/* /// use futures_util::future::poll_fn; */
/* /// use tokio::task; */
/* /// use std::{cell::UnsafeCell, pin::Pin}; */
/* /// */
/* /// let ring = UnsafeCell::new(RingFuturized::new()); */
/* /// let read_lease = poll_fn(|cx| unsafe { &mut *ring.get()
 * }.poll_read(cx, 5)); */
/* /// { */
/* ///   let mut write_lease = poll_fn(|cx| { */
/* ///     unsafe { &mut *ring.get() }.poll_write(cx, 20) */
/* ///   }).await; */
/* ///   write_lease.truncate(5).copy_from_slice(b"hello"); */
/* /// } */
/* /// { */
/* ///   let read_lease = read_lease.await; */
/* ///   assert_eq!("hello", std::str::from_utf8(&read_lease).unwrap()); */
/* /// } */
/* /// # })} */
/* /// ``` */
/* pub struct RingFuturized { */
/* buf: mem::ManuallyDrop<Ring>, */
/* read_wakers: Pusher<Waker>, */
/* write_wakers: Pusher<Waker>, */
/* } */

/* impl ops::Drop for RingFuturized { */
/* fn drop(&mut self) { */
/* let Self { */
/* buf, */
/* read_wakers, */
/* write_wakers, */
/* } = self; */
/* for waker in read_wakers */
/* .take_owned() */
/* .into_iter() */
/* .chain(write_wakers.take_owned().into_iter()) */
/* { */
/* waker.wake(); */
/* } */
/* return_ring(unsafe { mem::ManuallyDrop::take(buf) }); */
/* } */
/* } */

/* impl RingFuturized { */
/* #[inline] */
/* pub fn capacity(&self) -> usize { self.buf.capacity() } */

/* pub fn new() -> Self { */
/* let ring = get_or_create_ring(|| Ring::with_capacity(8 * 1024)); */
/* Self { */
/* buf: mem::ManuallyDrop::new(ring), */
/* read_wakers: Pusher::<Waker>::new(), */
/* write_wakers: Pusher::<Waker>::new(), */
/* } */
/* } */

/* /\* pub fn wrap_ring(buf: Ring) -> Self { *\/ */
/* /\* Self { *\/ */
/* /\* buf: Arc::new(buf), *\/ */
/* /\* read_wakers: Arc::new(Pusher::<Waker>::new()), *\/ */
/* /\* write_wakers: Arc::new(Pusher::<Waker>::new()), *\/ */
/* /\* } *\/ */
/* /\* } *\/ */

/* /\* pub fn poll_read_until_no_space( *\/ */
/* /\* &mut self, *\/ */
/* /\* cx: &mut Context<'_>, *\/ */
/* /\* ) -> Poll<Option<ReadPermitFuturized>> { *\/ */
/* /\* match self.buf.request_read_lease(self.capacity()) { *\/ */
/* /\* Lease::NoSpace => Poll::Ready(None), *\/ */
/* /\* Lease::PossiblyTaken => { *\/ */
/* /\* self.read_wakers.push(cx.waker().clone()); *\/ */
/* /\* Poll::Pending *\/ */
/* /\* } *\/ */
/* /\* Lease::Taken(permit) => Poll::Ready(Some(ReadPermitFuturized::for_buf(
 * *\/ */
/* /\* permit, *\/ */
/* /\* self.read_wakers.clone(), *\/ */
/* /\* self.write_wakers.clone(), *\/ */
/* /\* ))), *\/ */
/* /\* } *\/ */
/* /\* } *\/ */

/* pub fn poll_read( */
/* &mut self, */
/* cx: &mut Context<'_>, */
/* requested_length: usize, */
/* ) -> Poll<ReadPermitFuturized<'_>> { */
/* match self.buf.request_read_lease(requested_length) { */
/* Lease::NoSpace | Lease::PossiblyTaken => { */
/* self.read_wakers.push(cx.waker().clone()); */
/* Poll::Pending */
/* }, */
/* Lease::Taken(permit) => Poll::Ready(ReadPermitFuturized::for_buf( */
/* permit, */
/* &self.read_wakers, */
/* &self.write_wakers, */
/* )), */
/* } */
/* } */

/* pub fn poll_write( */
/* &mut self, */
/* cx: &mut Context<'_>, */
/* requested_length: usize, */
/* ) -> Poll<WritePermitFuturized<'_>> { */
/* match self.buf.request_write_lease(requested_length) { */
/* Lease::NoSpace | Lease::PossiblyTaken => { */
/* self.write_wakers.push(cx.waker().clone()); */
/* Poll::Pending */
/* }, */
/* Lease::Taken(permit) => Poll::Ready(WritePermitFuturized::for_buf( */
/* permit, */
/* &self.read_wakers, */
/* &self.write_wakers, */
/* )), */
/* } */
/* } */
/* } */

/* pub struct ReadPermitFuturized<'a> { */
/* buf: mem::ManuallyDrop<ReadPermit<'a>>, */
/* read_wakers: &'a Pusher<Waker>, */
/* write_wakers: &'a Pusher<Waker>, */
/* } */

/* impl<'a> ReadPermitFuturized<'a> { */
/* pub(crate) fn for_buf( */
/* buf: ReadPermit<'a>, */
/* read_wakers: &'a Pusher<Waker>, */
/* write_wakers: &'a Pusher<Waker>, */
/* ) -> Self { */
/* Self { */
/* buf: mem::ManuallyDrop::new(buf), */
/* read_wakers, */
/* write_wakers, */
/* } */
/* } */
/* } */

/* impl<'a> ops::Drop for ReadPermitFuturized<'a> { */
/* fn drop(&mut self) { */
/* let Self { */
/* buf, */
/* read_wakers, */
/* write_wakers, */
/* } = self; */
/* let was_empty = buf.is_empty(); */
/* /\* Drop the ReadPermit first to close out the owned region in the parent
 * Ring */
/* * before waking up any tasks. *\/ */
/* unsafe { */
/* mem::ManuallyDrop::drop(buf); */
/* } */
/* /\* Notify any blocked readers. *\/ */
/* for waker in read_wakers.extract().into_iter() { */
/* waker.wake(); */
/* } */
/* if !was_empty { */
/* /\* Notify any blocked writers. *\/ */
/* for waker in write_wakers.extract().into_iter() { */
/* waker.wake(); */
/* } */
/* } */
/* } */
/* } */

/* impl<'a> AsRef<ReadPermit<'a>> for ReadPermitFuturized<'a> { */
/* #[inline] */
/* fn as_ref(&self) -> &ReadPermit<'a> { &self.buf } */
/* } */

/* impl<'a> AsMut<ReadPermit<'a>> for ReadPermitFuturized<'a> { */
/* #[inline] */
/* fn as_mut(&mut self) -> &mut ReadPermit<'a> { &mut self.buf } */
/* } */

/* impl<'a> ops::Deref for ReadPermitFuturized<'a> { */
/* type Target = ReadPermit<'a>; */

/* #[inline] */
/* fn deref(&self) -> &ReadPermit<'a> { &self.buf } */
/* } */

/* impl<'a> ops::DerefMut for ReadPermitFuturized<'a> { */
/* #[inline] */
/* fn deref_mut(&mut self) -> &mut ReadPermit<'a> { &mut self.buf } */
/* } */

/* pub struct WritePermitFuturized<'a> { */
/* buf: mem::ManuallyDrop<WritePermit<'a>>, */
/* read_wakers: &'a Pusher<Waker>, */
/* write_wakers: &'a Pusher<Waker>, */
/* } */

/* impl<'a> WritePermitFuturized<'a> { */
/* pub(crate) fn for_buf( */
/* buf: WritePermit<'a>, */
/* read_wakers: &'a Pusher<Waker>, */
/* write_wakers: &'a Pusher<Waker>, */
/* ) -> Self { */
/* Self { */
/* buf: mem::ManuallyDrop::new(buf), */
/* read_wakers, */
/* write_wakers, */
/* } */
/* } */
/* } */

/* impl<'a> ops::Drop for WritePermitFuturized<'a> { */
/* fn drop(&mut self) { */
/* let Self { */
/* buf, */
/* read_wakers, */
/* write_wakers, */
/* } = self; */
/* let was_empty = buf.is_empty(); */
/* /\* Drop the WritePermit first to close out the owned region in the parent */
/* * Ring before waking up any tasks. *\/ */
/* unsafe { */
/* mem::ManuallyDrop::drop(buf); */
/* } */
/* /\* Notify any blocked writers. *\/ */
/* for waker in write_wakers.extract().into_iter() { */
/* waker.wake(); */
/* } */
/* if !was_empty { */
/* /\* Notify any blocked readers. *\/ */
/* for waker in read_wakers.extract().into_iter() { */
/* waker.wake(); */
/* } */
/* } */
/* } */
/* } */

/* impl<'a> AsRef<WritePermit<'a>> for WritePermitFuturized<'a> { */
/* #[inline] */
/* fn as_ref(&self) -> &WritePermit<'a> { &self.buf } */
/* } */

/* impl<'a> AsMut<WritePermit<'a>> for WritePermitFuturized<'a> { */
/* #[inline] */
/* fn as_mut(&mut self) -> &mut WritePermit<'a> { &mut self.buf } */
/* } */

/* impl<'a> ops::Deref for WritePermitFuturized<'a> { */
/* type Target = WritePermit<'a>; */

/* #[inline] */
/* fn deref(&self) -> &WritePermit<'a> { &self.buf } */
/* } */

/* impl<'a> ops::DerefMut for WritePermitFuturized<'a> { */
/* #[inline] */
/* fn deref_mut(&mut self) -> &mut WritePermit<'a> { &mut self.buf } */
/* } */
/* } */

/* impl std::io::Read for RingBuffer { */
/* fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> { */
/* debug_assert!(!buf.is_empty()); */
/* /\* TODO: is this sufficient to make underflow unambiguous? *\/ */
/* static_assertions::const_assert!(N < (usize::MAX >> 1)); */

/* let requested_length: usize = cmp::min(N, buf.len()); */
/* self.remaining */
/* } */
/* } */
