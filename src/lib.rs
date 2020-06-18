//! Straightforward Linux AIO using Futures/async/await.
//!
//! # Example
//!
//! Use libaiofut to schedule writes to a file:
//!
//! ```rust
//! use futures::{executor::LocalPool, future::FutureExt, task::LocalSpawnExt};
//! use libaiofut::AIOBuilder;
//! use std::os::unix::io::AsRawFd;
//! let mut aiomgr = AIOBuilder::default().build().unwrap();
//! let file = std::fs::OpenOptions::new()
//!     .read(true)
//!     .write(true)
//!     .create(true)
//!     .truncate(true)
//!     .open("test")
//!     .unwrap();
//! let fd = file.as_raw_fd();
//! // keep all returned futures in a vector
//! let ws = vec![(0, "hello"), (5, "world"), (2, "xxxx")]
//!     .into_iter()
//!     .map(|(off, s)| aiomgr.write(fd, off, s.as_bytes().into(), None))
//!     .collect::<Vec<_>>();
//! // here we use futures::executor::LocalPool to poll all futures
//! let mut pool = LocalPool::new();
//! let spawner = pool.spawner();
//! for w in ws.into_iter() {
//!     let h = spawner.spawn_local_with_handle(w).unwrap().map(|r| {
//!         println!("wrote {} bytes", r.unwrap().0);
//!     });
//!     spawner.spawn_local(h).unwrap();
//! }
//! pool.run();
//! ```

mod abi;
use parking_lot::Mutex;
use std::collections::{hash_map, HashMap};
use std::os::unix::io::RawFd;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicPtr, Ordering},
    Arc,
};

const LIBAIO_EAGAIN: libc::c_int = -libc::EAGAIN;
const LIBAIO_ENOMEM: libc::c_int = -libc::ENOMEM;
const LIBAIO_ENOSYS: libc::c_int = -libc::ENOSYS;

#[derive(Debug)]
pub enum Error {
    MaxEventsTooLarge,
    LowKernelRes,
    NotSupported,
    OtherError,
}

// NOTE: I assume it io_context_t is thread-safe, no?
struct AIOContext(abi::IOContextPtr);
unsafe impl Sync for AIOContext {}
unsafe impl Send for AIOContext {}

impl std::ops::Deref for AIOContext {
    type Target = abi::IOContextPtr;
    fn deref(&self) -> &abi::IOContextPtr {
        &self.0
    }
}

impl AIOContext {
    fn new(maxevents: u32) -> Result<Self, Error> {
        let mut ctx = std::ptr::null_mut();
        unsafe {
            match abi::io_setup(maxevents as libc::c_int, &mut ctx) {
                0 => Ok(()),
                LIBAIO_EAGAIN => Err(Error::MaxEventsTooLarge),
                LIBAIO_ENOMEM => Err(Error::LowKernelRes),
                LIBAIO_ENOSYS => Err(Error::NotSupported),
                _ => Err(Error::OtherError),
            }
            .and(Ok(AIOContext(ctx)))
        }
    }
}

/// Represent the necessary data for an AIO operation. Memory-safe when moved.
pub struct AIO {
    // hold the buffer used by iocb
    data: Option<Box<[u8]>>,
    iocb: AtomicPtr<abi::IOCb>,
    id: u64,
}

impl AIO {
    fn new(
        id: u64,
        fd: RawFd,
        off: u64,
        data: Box<[u8]>,
        priority: u16,
        flags: u32,
        opcode: abi::IOCmd,
    ) -> Self {
        let mut iocb = Box::new(abi::IOCb::default());
        iocb.aio_fildes = fd as u32;
        iocb.aio_lio_opcode = opcode as u16;
        iocb.aio_reqprio = priority;
        iocb.aio_buf = data.as_ptr() as u64;
        iocb.aio_nbytes = data.len() as u64;
        iocb.aio_offset = off;
        iocb.aio_flags = flags;
        iocb.aio_data = id;
        let iocb = AtomicPtr::new(Box::into_raw(iocb));
        let data = Some(data);
        AIO { iocb, id, data }
    }
}

impl Drop for AIO {
    fn drop(&mut self) {
        unsafe {
            drop(Box::from_raw(self.iocb.load(Ordering::Acquire)));
        }
    }
}

/// The result of an AIO operation: the number of bytes written on success,
/// or the errno on failure.
pub type AIOResult = Result<(usize, Box<[u8]>), i32>;

/// Represents a scheduled (future) asynchronous I/O operation, which gets executed (resolved)
/// automatically.
pub struct AIOFuture {
    notifier: Arc<AIONotifier>,
    aio_id: u64,
}

impl std::future::Future for AIOFuture {
    type Output = AIOResult;
    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context) -> std::task::Poll<Self::Output> {
        if let Some(ret) = self.notifier.poll(self.aio_id, cx.waker()) {
            std::task::Poll::Ready(ret)
        } else {
            std::task::Poll::Pending
        }
    }
}

impl Drop for AIOFuture {
    fn drop(&mut self) {
        self.notifier.dropped(self.aio_id)
    }
}

enum AIOState {
    FutureInit(AIO, bool),
    FuturePending(AIO, std::task::Waker, bool),
    FutureDone(AIOResult),
}

/// The state machine for finished AIO operations and wakes up the futures.
pub struct AIONotifier {
    waiting: Mutex<HashMap<u64, AIOState>>,
    io_ctx: AIOContext,
}

impl AIONotifier {
    fn register_notify(&self, id: u64, state: AIOState) {
        let mut waiting = self.waiting.lock();
        assert!(waiting.insert(id, state).is_none());
    }

    fn dropped(&self, id: u64) {
        let mut waiting = self.waiting.lock();
        match waiting.entry(id) {
            hash_map::Entry::Occupied(mut e) => match e.get_mut() {
                AIOState::FutureInit(_, dropped) => *dropped = true,
                AIOState::FuturePending(_, _, dropped) => *dropped = true,
                AIOState::FutureDone(_) => {
                    e.remove();
                }
            },
            _ => (),
        }
    }

    fn poll(&self, id: u64, waker: &std::task::Waker) -> Option<AIOResult> {
        let mut waiting = self.waiting.lock();
        match waiting.entry(id) {
            hash_map::Entry::Occupied(e) => {
                let v = e.remove();
                match v {
                    AIOState::FutureInit(aio, _) => {
                        waiting.insert(id, AIOState::FuturePending(aio, waker.clone(), false));
                        None
                    }
                    AIOState::FuturePending(aio, waker, dropped) => {
                        waiting.insert(id, AIOState::FuturePending(aio, waker, dropped));
                        None
                    }
                    AIOState::FutureDone(res) => Some(res),
                }
            }
            _ => unreachable!(),
        }
    }

    fn finish(&self, id: u64, res: i64) {
        let mut w = self.waiting.lock();
        match w.entry(id) {
            hash_map::Entry::Occupied(e) => match e.remove() {
                AIOState::FutureInit(mut aio, dropped) => {
                    if !dropped {
                        w.insert(
                            id,
                            AIOState::FutureDone(if res >= 0 {
                                Ok((res as usize, aio.data.take().unwrap()))
                            } else {
                                Err(-res as i32)
                            }),
                        );
                    }
                }
                AIOState::FuturePending(mut aio, waker, dropped) => {
                    if !dropped {
                        w.insert(
                            id,
                            AIOState::FutureDone(if res >= 0 {
                                Ok((res as usize, aio.data.take().unwrap()))
                            } else {
                                Err(-res as i32)
                            }),
                        );
                        waker.wake();
                    }
                }
                AIOState::FutureDone(ret) => {
                    w.insert(id, AIOState::FutureDone(ret));
                }
            },
            _ => unreachable!(),
        }
    }
}

pub struct AIOBuilder {
    max_events: u32,
    max_nwait: u16,
    max_nbatched: usize,
    timeout: Option<u32>,
}

impl Default for AIOBuilder {
    fn default() -> Self {
        AIOBuilder {
            max_events: 128,
            max_nwait: 128,
            max_nbatched: 128,
            timeout: None,
        }
    }
}

impl AIOBuilder {
    /// Maximum concurrent async IO operations.
    pub fn max_events(&mut self, v: u32) -> &Self {
        self.max_events = v;
        self
    }

    /// Maximum complete IOs per poll.
    pub fn max_nwait(&mut self, v: u16) -> &Self {
        self.max_nwait = v;
        self
    }

    /// Maximum number of IOs per submission.
    pub fn max_nbatched(&mut self, v: usize) -> &Self {
        self.max_nbatched = v;
        self
    }

    /// Timeout for a polling iteration (default is None).
    pub fn timeout(&mut self, sec: u32) -> &Self {
        self.timeout = Some(sec);
        self
    }

    /// Build an AIOManager object based on the configuration (and auto-start the background IO
    /// scheduling thread).
    pub fn build(&mut self) -> Result<AIOManager, Error> {
        let (scheduler_in, scheduler_out) = new_batch_scheduler(self.max_nbatched);
        let (exit_s, exit_r) = crossbeam_channel::bounded(0);

        let notifier = Arc::new(AIONotifier {
            io_ctx: AIOContext::new(self.max_events)?,
            waiting: Mutex::new(HashMap::new()),
        });
        let mut aiomgr = AIOManager {
            notifier,
            listener: None,
            scheduler_in,
            exit_s,
        };
        aiomgr.start(scheduler_out, exit_r, self.max_nwait, self.timeout)?;
        Ok(aiomgr)
    }
}

/// Manager all AIOs.
pub struct AIOManager {
    notifier: Arc<AIONotifier>,
    scheduler_in: AIOBatchSchedulerIn,
    listener: Option<std::thread::JoinHandle<()>>,
    exit_s: crossbeam_channel::Sender<()>,
}

impl AIOManager {
    fn start(
        &mut self,
        mut scheduler_out: AIOBatchSchedulerOut,
        exit_r: crossbeam_channel::Receiver<()>,
        max_nwait: u16,
        timeout: Option<u32>,
    ) -> Result<(), Error> {
        let n = self.notifier.clone();
        self.listener = Some(std::thread::spawn(move || {
            let mut timespec = timeout.and_then(|sec: u32| {
                Some(libc::timespec {
                    tv_sec: sec as i64,
                    tv_nsec: 0,
                })
            });
            let mut ongoing = 0;
            loop {
                // try to quiesce
                if ongoing == 0 && scheduler_out.is_empty() {
                    let mut sel = crossbeam_channel::Select::new();
                    sel.recv(&exit_r);
                    sel.recv(&scheduler_out.get_receiver());
                    if sel.ready() == 0 {
                        exit_r.recv().unwrap();
                        break;
                    }
                }
                // submit as many aios as possible
                loop {
                    let nacc = scheduler_out.submit(&n);
                    ongoing += nacc;
                    if nacc == 0 {
                        break;
                    }
                }
                // no need to wait if there is no progress
                if ongoing == 0 {
                    continue;
                }
                // then block on any finishing aios
                let mut events = vec![abi::IOEvent::default(); max_nwait as usize];
                let ret = unsafe {
                    abi::io_getevents(
                        *n.io_ctx,
                        1,
                        max_nwait as i64,
                        events.as_mut_ptr(),
                        timespec
                            .as_mut()
                            .and_then(|t| Some(t as *mut libc::timespec))
                            .unwrap_or(std::ptr::null_mut()),
                    )
                };
                // TODO: AIO fatal error handling
                // avoid empty slice
                if ret == 0 {
                    continue;
                }
                assert!(ret > 0);
                ongoing -= ret as usize;
                for ev in events[..ret as usize].iter() {
                    n.finish(ev.data as u64, ev.res);
                }
            }
        }));
        Ok(())
    }

    pub fn read(&self, fd: RawFd, offset: u64, length: usize, priority: Option<u16>) -> AIOFuture {
        let priority = priority.unwrap_or(0);
        let mut data = Vec::new();
        data.resize(length, 0);
        let data = data.into_boxed_slice();
        let aio = AIO::new(
            self.scheduler_in.next_id(),
            fd,
            offset,
            data,
            priority,
            0,
            abi::IOCmd::PRead,
        );
        self.scheduler_in.schedule(aio, &self.notifier)
    }

    pub fn write(
        &self,
        fd: RawFd,
        offset: u64,
        data: Box<[u8]>,
        priority: Option<u16>,
    ) -> AIOFuture {
        let priority = priority.unwrap_or(0);
        let aio = AIO::new(
            self.scheduler_in.next_id(),
            fd,
            offset,
            data,
            priority,
            0,
            abi::IOCmd::PWrite,
        );
        self.scheduler_in.schedule(aio, &self.notifier)
    }
}

impl Drop for AIOManager {
    fn drop(&mut self) {
        self.exit_s.send(()).unwrap();
        self.listener.take().unwrap().join().unwrap();
    }
}

pub struct AIOBatchSchedulerIn {
    queue_in: crossbeam_channel::Sender<AtomicPtr<abi::IOCb>>,
    last_id: std::cell::Cell<u64>,
}

pub struct AIOBatchSchedulerOut {
    queue_out: crossbeam_channel::Receiver<AtomicPtr<abi::IOCb>>,
    max_nbatched: usize,
    leftover: Vec<AtomicPtr<abi::IOCb>>,
}

impl AIOBatchSchedulerIn {
    fn schedule(&self, aio: AIO, notifier: &Arc<AIONotifier>) -> AIOFuture {
        let fut = AIOFuture {
            notifier: notifier.clone(),
            aio_id: aio.id,
        };
        let iocb = aio.iocb.load(Ordering::Acquire);
        notifier.register_notify(aio.id, AIOState::FutureInit(aio, false));
        self.queue_in.send(AtomicPtr::new(iocb)).unwrap();
        fut
    }

    fn next_id(&self) -> u64 {
        let id = self.last_id.get();
        self.last_id.set(id.wrapping_add(1));
        id
    }
}

impl AIOBatchSchedulerOut {
    fn get_receiver(&self) -> &crossbeam_channel::Receiver<AtomicPtr<abi::IOCb>> {
        &self.queue_out
    }
    fn is_empty(&self) -> bool {
        self.leftover.len() == 0
    }
    fn submit(&mut self, notifier: &AIONotifier) -> usize {
        let mut quota = self.max_nbatched;
        let mut pending = self
            .leftover
            .iter()
            .map(|p| p.load(Ordering::Acquire))
            .collect::<Vec<_>>();
        if pending.len() < quota {
            quota -= pending.len();
            while let Ok(iocb) = self.queue_out.try_recv() {
                pending.push(iocb.load(Ordering::Acquire));
                quota -= 1;
                if quota == 0 {
                    break;
                }
            }
        }
        if pending.len() == 0 {
            return 0;
        }
        let mut ret = unsafe {
            abi::io_submit(
                *notifier.io_ctx,
                pending.len() as i64,
                (&mut pending).as_mut_ptr(),
            )
        };
        if ret < 0 && ret == LIBAIO_EAGAIN {
            ret = 0
        }
        let nacc = ret as usize;
        self.leftover = (&pending[nacc..])
            .iter()
            .map(|p| AtomicPtr::new(*p))
            .collect::<Vec<_>>();
        nacc
    }
}

/// Create the scheduler that submits AIOs in batches.
fn new_batch_scheduler(max_nbatched: usize) -> (AIOBatchSchedulerIn, AIOBatchSchedulerOut) {
    let (queue_in, queue_out) = crossbeam_channel::unbounded();
    let bin = AIOBatchSchedulerIn {
        queue_in,
        last_id: std::cell::Cell::new(0),
    };
    let bout = AIOBatchSchedulerOut {
        queue_out,
        max_nbatched,
        leftover: Vec::new(),
    };
    (bin, bout)
}
