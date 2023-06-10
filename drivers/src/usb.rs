use crate::error;
use rusb::UsbContext;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Configuration {
    pub buffer_size: usize,
    pub ring_size: usize,
    pub transfer_queue_size: usize,
}

#[derive(thiserror::Error, Debug, Clone)]
pub enum Error {
    #[error("{0}")]
    Rusb(#[from] rusb::Error),

    #[error("device with serial not found")]
    Serial(String),

    #[error("device not found")]
    Device,

    #[error("ring size is smaller than or equal to transfer queue size")]
    ConfigurationSizes,

    #[error("ring overflow")]
    Overflow,

    #[error("control transfer error (expected {expected:?}, read {read:?})")]
    Mismatch { expected: Vec<u8>, read: Vec<u8> },
    // @DEV add special error for busy devices, should be ignored when scanning to support multiple cameras of the same type
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum Speed {
    Unknown,
    Low,
    Full,
    High,
    Super,
    SuperPlus,
}

impl From<rusb::Speed> for Speed {
    fn from(speed: rusb::Speed) -> Self {
        match speed {
            rusb::Speed::Low => Self::Low,
            rusb::Speed::Full => Self::Full,
            rusb::Speed::High => Self::High,
            rusb::Speed::Super => Self::Super,
            rusb::Speed::SuperPlus => Self::SuperPlus,
            rusb::Speed::Unknown | _ => Self::Unknown,
        }
    }
}

impl ToString for Speed {
    fn to_string(&self) -> String {
        (match self {
            Self::Unknown => "USB Unknown speed",
            Self::Low => "USB 1.0 Low Speed (1.5 Mb/s)",
            Self::Full => "USB 1.1 Full Speed (12 Mb/s)",
            Self::High => "USB 2.0 High Speed (480 Mb/s)",
            Self::Super => "USB 3.0 SuperSpeed (5.0 Gb/s)",
            Self::SuperPlus => "USB 3.1 SuperSpeed+ (10.0 Gb/s)",
        })
        .to_owned()
    }
}

pub fn assert_control_transfer(
    handle: &rusb::DeviceHandle<rusb::Context>,
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    expected_buffer: &[u8],
    timeout: std::time::Duration,
) -> Result<(), Error> {
    let mut buffer = vec![0; expected_buffer.len()];
    let read = handle.read_control(request_type, request, value, index, &mut buffer, timeout)?;
    buffer.truncate(read);
    if expected_buffer == &buffer[..] {
        Ok(())
    } else {
        Err(Error::Mismatch {
            expected: Vec::from(expected_buffer),
            read: buffer,
        })
    }
}

extern "system" {
    pub fn libusb_dev_mem_alloc(
        dev_handle: *mut libusb1_sys::libusb_device_handle,
        length: libc::ssize_t,
    ) -> *mut libc::c_uchar;

    pub fn libusb_dev_mem_free(
        dev_handle: *mut libusb1_sys::libusb_device_handle,
        buffer: *mut libc::c_uchar,
        length: libc::ssize_t,
    ) -> *mut libc::c_int;
}

struct BufferData(std::ptr::NonNull<u8>);

unsafe impl Send for BufferData {}
unsafe impl Sync for BufferData {}

impl BufferData {
    fn as_ptr(&self) -> *mut u8 {
        self.0.as_ptr()
    }
}

struct Buffer {
    system_time: std::time::SystemTime,
    data: BufferData,
    length: usize,
    capacity: usize,
    dma: bool,
}

pub struct EventLoop {
    context: rusb::Context,
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl EventLoop {
    pub fn new<IntoError>(
        timeout: std::time::Duration,
        error_flag: error::Flag<IntoError>,
    ) -> Result<Self, Error>
    where
        IntoError: From<Error> + Clone + Send + 'static,
    {
        let context = rusb::Context::new()?;
        let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let thread_running = running.clone();
        let thread_context = context.clone();
        Ok(Self {
            context,
            thread: Some(std::thread::spawn(move || {
                while thread_running.load(std::sync::atomic::Ordering::Acquire) {
                    if let Err(handle_events_error) = thread_context.handle_events(Some(timeout)) {
                        error_flag.store_if_not_set(Error::from(handle_events_error));
                    }
                }
            })),
            running,
        })
    }

    pub fn context(&self) -> &rusb::Context {
        &self.context
    }
}

impl Drop for EventLoop {
    fn drop(&mut self) {
        self.running
            .store(false, std::sync::atomic::Ordering::Release);
        if let Some(thread) = self.thread.take() {
            // unwrap: not joining self
            thread.join().unwrap();
        }
    }
}

enum TransferStatus {
    Active,
    Complete,
    Cancelling,
    Deallocated,
}

struct SharedRingContext {
    read: usize,
    write_range: (usize, usize),
    transfer_statuses: Vec<TransferStatus>,
    buffers: Vec<Buffer>,
}

struct RingContext {
    on_error: Box<dyn Fn(Error) + Send + Sync + 'static>,
    shared: std::sync::Mutex<SharedRingContext>,
    shared_condvar: std::sync::Condvar,
}

struct LibusbTransfer(std::ptr::NonNull<libusb1_sys::libusb_transfer>);

unsafe impl Send for LibusbTransfer {}

impl LibusbTransfer {
    unsafe fn as_mut(&mut self) -> &mut libusb1_sys::libusb_transfer {
        self.0.as_mut()
    }

    fn as_ptr(&self) -> *mut libusb1_sys::libusb_transfer {
        self.0.as_ptr()
    }
}

pub struct Ring {
    transfers: Vec<LibusbTransfer>,
    handle: std::sync::Arc<rusb::DeviceHandle<rusb::Context>>,
    #[allow(dead_code)]
    event_loop: std::sync::Arc<EventLoop>,
    context: std::sync::Arc<RingContext>,
}

pub enum TransferType {
    Control(std::time::Duration),
    Isochronous {
        endpoint: u8,
        packets: u32,
        timeout: std::time::Duration,
    },
    Bulk {
        endpoint: u8,
        timeout: std::time::Duration,
    },
    Interrupt {
        endpoint: u8,
        timeout: std::time::Duration,
    },
    BulkStream {
        endpoint: u8,
        stream_id: u32,
        timeout: std::time::Duration,
    },
}

pub struct TransferProperties {
    pub transfer_type: TransferType,
    pub timeout: std::time::Duration,
}

struct TransferContext {
    ring: std::sync::Arc<RingContext>,
    transfer_index: usize,
}

#[no_mangle]
extern "system" fn usb_transfer_callback(transfer_pointer: *mut libusb1_sys::libusb_transfer) {
    let now = std::time::SystemTime::now();
    let mut resubmit = false;
    {
        // unsafe: transfer is not null (libusb callback)
        let transfer = unsafe { &mut *transfer_pointer };
        let context = transfer.user_data;
        assert!(!context.is_null(), "context is null");
        // unsafe: context is a *mut TransferContext
        let context = unsafe { &mut *(context as *mut TransferContext) };
        let mut error = None;
        {
            // unwrap: mutex is not poisonned
            let mut shared = context.ring.shared.lock().unwrap();
            match shared.transfer_statuses[context.transfer_index] {
                TransferStatus::Active => match transfer.status {
                    libusb1_sys::constants::LIBUSB_TRANSFER_COMPLETED
                    | libusb1_sys::constants::LIBUSB_TRANSFER_TIMED_OUT => {
                        if shared.write_range.1 == shared.read {
                            error = Some(Error::Overflow);
                            shared.transfer_statuses[context.transfer_index] =
                                TransferStatus::Complete;
                        } else {
                            let active_buffer = shared.write_range.0;
                            shared.buffers[active_buffer].system_time = now;
                            shared.buffers[active_buffer].length = transfer.actual_length as usize;
                            shared.write_range.0 =
                                (shared.write_range.0 + 1) % shared.buffers.len();
                            shared.write_range.1 =
                                (shared.write_range.1 + 1) % shared.buffers.len();
                            transfer.buffer = shared.buffers[shared.write_range.1].data.as_ptr();
                            transfer.length = shared.buffers[shared.write_range.1].capacity as i32;
                            resubmit = true;
                            context.ring.shared_condvar.notify_one();
                        }
                    }
                    status @ (libusb1_sys::constants::LIBUSB_TRANSFER_ERROR
                    | libusb1_sys::constants::LIBUSB_TRANSFER_CANCELLED
                    | libusb1_sys::constants::LIBUSB_TRANSFER_STALL
                    | libusb1_sys::constants::LIBUSB_TRANSFER_NO_DEVICE
                    | libusb1_sys::constants::LIBUSB_TRANSFER_OVERFLOW) => {
                        if shared.write_range.1 != shared.read {
                            let active_buffer = shared.write_range.0;
                            shared.buffers[active_buffer].system_time = now;
                            shared.buffers[active_buffer].length = transfer.actual_length as usize;
                            shared.write_range.0 =
                                (shared.write_range.0 + 1) % shared.buffers.len();
                            shared.write_range.1 =
                                (shared.write_range.1 + 1) % shared.buffers.len();
                            context.ring.shared_condvar.notify_one();
                        }
                        shared.transfer_statuses[context.transfer_index] = TransferStatus::Complete;
                        error = Some(
                            match status {
                                libusb1_sys::constants::LIBUSB_TRANSFER_ERROR
                                | libusb1_sys::constants::LIBUSB_TRANSFER_CANCELLED => {
                                    rusb::Error::Io
                                }
                                libusb1_sys::constants::LIBUSB_TRANSFER_STALL => rusb::Error::Pipe,
                                libusb1_sys::constants::LIBUSB_TRANSFER_NO_DEVICE => {
                                    rusb::Error::NoDevice
                                }
                                libusb1_sys::constants::LIBUSB_TRANSFER_OVERFLOW => {
                                    rusb::Error::Overflow
                                }
                                _ => rusb::Error::Other,
                            }
                            .into(),
                        );
                    }
                    unknown_transfer_status => {
                        panic!("unknown transfer status {unknown_transfer_status}")
                    }
                },
                TransferStatus::Cancelling => match transfer.status {
                    libusb1_sys::constants::LIBUSB_TRANSFER_COMPLETED
                    | libusb1_sys::constants::LIBUSB_TRANSFER_TIMED_OUT
                    | libusb1_sys::constants::LIBUSB_TRANSFER_ERROR
                    | libusb1_sys::constants::LIBUSB_TRANSFER_CANCELLED
                    | libusb1_sys::constants::LIBUSB_TRANSFER_STALL
                    | libusb1_sys::constants::LIBUSB_TRANSFER_NO_DEVICE => {
                        if shared.write_range.1 != shared.read {
                            let active_buffer = shared.write_range.0;
                            shared.buffers[active_buffer].system_time = now;
                            shared.buffers[active_buffer].length = transfer.actual_length as usize;
                            shared.write_range.0 =
                                (shared.write_range.0 + 1) % shared.buffers.len();
                            shared.write_range.1 =
                                (shared.write_range.1 + 1) % shared.buffers.len();
                            context.ring.shared_condvar.notify_one();
                        }
                        shared.transfer_statuses[context.transfer_index] = TransferStatus::Complete;
                    }
                    unknown_transfer_status => {
                        panic!("unknown transfer status {unknown_transfer_status}")
                    }
                },
                TransferStatus::Complete => {
                    panic!("callback called for a transfer marked as complete")
                }
                TransferStatus::Deallocated => {
                    panic!("callback called for a transfer marked as deallocated")
                }
            }
        }
        if let Some(error) = error {
            (context.ring.on_error)(error);
        }
    }
    if resubmit {
        // unsafe: libusb_alloc_transfer succeeded
        match unsafe { libusb1_sys::libusb_submit_transfer(transfer_pointer) } {
            0 => (),
            submit_transfer_status => {
                // unsafe: transfer is not null (libusb callback)
                let transfer = unsafe { &mut *transfer_pointer };
                transfer.flags = 0;
                let context = transfer.user_data;
                assert!(!context.is_null(), "context is null");
                // unsafe: context is a *mut TransferContext
                let context = unsafe { &mut *(context as *mut TransferContext) };
                (context.ring.on_error)(
                    match submit_transfer_status {
                        libusb1_sys::constants::LIBUSB_ERROR_IO => rusb::Error::Io,
                        libusb1_sys::constants::LIBUSB_ERROR_INVALID_PARAM => {
                            rusb::Error::InvalidParam
                        }
                        libusb1_sys::constants::LIBUSB_ERROR_ACCESS => rusb::Error::Access,
                        libusb1_sys::constants::LIBUSB_ERROR_NO_DEVICE => rusb::Error::NoDevice,
                        libusb1_sys::constants::LIBUSB_ERROR_NOT_FOUND => rusb::Error::NotFound,
                        libusb1_sys::constants::LIBUSB_ERROR_BUSY => rusb::Error::Busy,
                        libusb1_sys::constants::LIBUSB_ERROR_TIMEOUT => rusb::Error::Timeout,
                        libusb1_sys::constants::LIBUSB_ERROR_OVERFLOW => rusb::Error::Overflow,
                        libusb1_sys::constants::LIBUSB_ERROR_PIPE => rusb::Error::Pipe,
                        libusb1_sys::constants::LIBUSB_ERROR_INTERRUPTED => {
                            rusb::Error::Interrupted
                        }
                        libusb1_sys::constants::LIBUSB_ERROR_NO_MEM => rusb::Error::NoMem,
                        libusb1_sys::constants::LIBUSB_ERROR_NOT_SUPPORTED => {
                            rusb::Error::NotSupported
                        }
                        _ => rusb::Error::Other,
                    }
                    .into(),
                );
            }
        }
    }
}

impl Ring {
    pub fn new<OnError>(
        handle: std::sync::Arc<rusb::DeviceHandle<rusb::Context>>,
        configuration: &Configuration,
        on_error: OnError,
        event_loop: std::sync::Arc<EventLoop>,
        transfer_type: TransferType,
    ) -> Result<Self, Error>
    where
        OnError: Fn(Error) + Send + Sync + 'static,
    {
        assert!(
            handle.context() == event_loop.context(),
            "handle and event_loop must have the same context"
        );
        if configuration.ring_size <= configuration.transfer_queue_size {
            return Err(Error::ConfigurationSizes);
        }
        let mut buffers = Vec::new();
        buffers.reserve_exact(configuration.ring_size);
        for _ in 0..configuration.ring_size {
            // unsafe: libusb wrapper
            let dma_buffer = unsafe {
                libusb_dev_mem_alloc(handle.as_raw(), configuration.buffer_size as libc::ssize_t)
            } as *mut u8;
            if dma_buffer.is_null() {
                buffers.push(Buffer {
                    system_time: std::time::SystemTime::now(),
                    data: BufferData(
                        std::ptr::NonNull::new(
                            // unsafe: alloc wrapper
                            // std::alloc::Layout::from_size_align_unchecked
                            // - align must not be zero
                            // - align must be a power of two
                            // - size, when rounded up to the nearest multiple of align, must not overflow isize
                            unsafe {
                                std::alloc::alloc(std::alloc::Layout::from_size_align_unchecked(
                                    configuration.buffer_size,
                                    1,
                                ))
                            },
                        )
                        .ok_or(rusb::Error::NoMem)?,
                    ),
                    length: 0,
                    capacity: configuration.buffer_size,
                    dma: true,
                });
            } else {
                buffers.push(Buffer {
                    system_time: std::time::SystemTime::now(),
                    // unsafe: dma_buffer is not null
                    data: BufferData(unsafe { std::ptr::NonNull::new_unchecked(dma_buffer) }),
                    length: 0,
                    capacity: configuration.buffer_size,
                    dma: true,
                });
            }
        }
        let mut transfer_statuses = Vec::new();
        transfer_statuses.reserve_exact(configuration.transfer_queue_size);
        for _ in 0..configuration.transfer_queue_size {
            transfer_statuses.push(TransferStatus::Active);
        }
        let context = std::sync::Arc::new(RingContext {
            on_error: Box::new(on_error),
            shared: std::sync::Mutex::new(SharedRingContext {
                read: buffers.len() - 1,
                write_range: (0, configuration.transfer_queue_size),
                transfer_statuses,
                buffers,
            }),
            shared_condvar: std::sync::Condvar::new(),
        });
        let mut transfers: Vec<LibusbTransfer> = Vec::new();
        transfers.reserve_exact(configuration.transfer_queue_size);
        {
            // unwrap: mutex is not poisonned
            let shared = context.shared.lock().unwrap();
            for index in 0..configuration.transfer_queue_size {
                // unsafe: libusb1_sys wrapper
                let mut transfer = match std::ptr::NonNull::new(unsafe {
                    libusb1_sys::libusb_alloc_transfer(0)
                }) {
                    Some(transfer) => LibusbTransfer(transfer),
                    None => {
                        for other_index in 0..index {
                            // unsafe: transfer is allocated and user_data is an allocated *mut TransferContext
                            unsafe {
                                Box::from_raw(
                                    (transfers[other_index].as_mut()).user_data
                                        as *mut TransferContext,
                                )
                            };
                            // unsafe: transfer is allocated
                            unsafe {
                                libusb1_sys::libusb_free_transfer(transfers[other_index].as_ptr())
                            };
                        }
                        return Err(rusb::Error::NoMem.into());
                    }
                };
                let transfer_context = Box::new(TransferContext {
                    ring: context.clone(),
                    transfer_index: index,
                });
                let transfer_context_pointer = Box::into_raw(transfer_context);
                match transfer_type {
                    // unsafe: libusb_alloc_transfer succeeded
                    TransferType::Control(timeout) => unsafe {
                        libusb1_sys::libusb_fill_control_transfer(
                            transfer.as_ptr(),
                            handle.as_raw(),
                            shared.buffers[index].data.as_ptr(),
                            usb_transfer_callback,
                            transfer_context_pointer as *mut libc::c_void,
                            timeout.as_millis() as libc::c_uint,
                        )
                    },
                    // unsafe: libusb_alloc_transfer succeeded
                    TransferType::Isochronous {
                        endpoint,
                        packets,
                        timeout,
                    } => unsafe {
                        libusb1_sys::libusb_fill_iso_transfer(
                            transfer.as_ptr(),
                            handle.as_raw(),
                            endpoint,
                            shared.buffers[index].data.as_ptr(),
                            shared.buffers[index].capacity as libc::c_int,
                            packets as libc::c_int,
                            usb_transfer_callback,
                            transfer_context_pointer as *mut libc::c_void,
                            timeout.as_millis() as libc::c_uint,
                        )
                    },
                    // unsafe: libusb_alloc_transfer succeeded
                    TransferType::Bulk { endpoint, timeout } => unsafe {
                        libusb1_sys::libusb_fill_bulk_transfer(
                            transfer.as_ptr(),
                            handle.as_raw(),
                            endpoint,
                            shared.buffers[index].data.as_ptr(),
                            shared.buffers[index].capacity as libc::c_int,
                            usb_transfer_callback,
                            transfer_context_pointer as *mut libc::c_void,
                            timeout.as_millis() as libc::c_uint,
                        )
                    },
                    // unsafe: libusb_alloc_transfer succeeded
                    TransferType::Interrupt { endpoint, timeout } => unsafe {
                        libusb1_sys::libusb_fill_interrupt_transfer(
                            transfer.as_ptr(),
                            handle.as_raw(),
                            endpoint,
                            shared.buffers[index].data.as_ptr(),
                            shared.buffers[index].capacity as libc::c_int,
                            usb_transfer_callback,
                            transfer_context_pointer as *mut libc::c_void,
                            timeout.as_millis() as libc::c_uint,
                        )
                    },
                    // unsafe: libusb_alloc_transfer succeeded
                    TransferType::BulkStream {
                        endpoint,
                        stream_id,
                        timeout,
                    } => unsafe {
                        libusb1_sys::libusb_fill_bulk_stream_transfer(
                            transfer.as_ptr(),
                            handle.as_raw(),
                            endpoint,
                            stream_id,
                            shared.buffers[index].data.as_ptr(),
                            shared.buffers[index].capacity as libc::c_int,
                            usb_transfer_callback,
                            transfer_context_pointer as *mut libc::c_void,
                            timeout.as_millis() as libc::c_uint,
                        )
                    },
                }
                // unsafe: libusb_alloc_transfer succeeded
                unsafe {
                    transfer.as_mut().flags = 0; // !LIBUSB_TRANSFER_SHORT_NOT_OK
                                                 // !LIBUSB_TRANSFER_FREE_BUFFER
                                                 // !LIBUSB_TRANSFER_FREE_TRANSFER
                                                 // !LIBUSB_TRANSFER_ADD_ZERO_PACKET
                }
                transfers.push(transfer);
            }
        }
        let result = Self {
            transfers,
            handle,
            event_loop,
            context,
        };
        for (index, transfer) in result.transfers.iter().enumerate() {
            // unsafe: libusb_alloc_transfer succeeded
            match unsafe { libusb1_sys::libusb_submit_transfer(transfer.as_ptr()) } {
                0 => (),
                submit_transfer_status => {
                    {
                        // unwrap: mutex is not poisonned
                        let mut shared = result.context.shared.lock().unwrap();
                        for rest_index in index..result.transfers.len() {
                            // dropping 'result' cancels transfers
                            // mark unscheduled transfers as complete to prevent un-needed cancelling
                            shared.transfer_statuses[rest_index] = TransferStatus::Complete;
                        }
                    }
                    return Err(match submit_transfer_status {
                        libusb1_sys::constants::LIBUSB_ERROR_IO => rusb::Error::Io,
                        libusb1_sys::constants::LIBUSB_ERROR_INVALID_PARAM => {
                            rusb::Error::InvalidParam
                        }
                        libusb1_sys::constants::LIBUSB_ERROR_ACCESS => rusb::Error::Access,
                        libusb1_sys::constants::LIBUSB_ERROR_NO_DEVICE => rusb::Error::NoDevice,
                        libusb1_sys::constants::LIBUSB_ERROR_NOT_FOUND => rusb::Error::NotFound,
                        libusb1_sys::constants::LIBUSB_ERROR_BUSY => rusb::Error::Busy,
                        libusb1_sys::constants::LIBUSB_ERROR_TIMEOUT => rusb::Error::Timeout,
                        libusb1_sys::constants::LIBUSB_ERROR_OVERFLOW => rusb::Error::Overflow,
                        libusb1_sys::constants::LIBUSB_ERROR_PIPE => rusb::Error::Pipe,
                        libusb1_sys::constants::LIBUSB_ERROR_INTERRUPTED => {
                            rusb::Error::Interrupted
                        }
                        libusb1_sys::constants::LIBUSB_ERROR_NO_MEM => rusb::Error::NoMem,
                        libusb1_sys::constants::LIBUSB_ERROR_NOT_SUPPORTED => {
                            rusb::Error::NotSupported
                        }
                        _ => rusb::Error::Other,
                    }
                    .into());
                }
            }
        }
        Ok(result)
    }
}

pub struct BufferView<'a> {
    pub system_time: std::time::SystemTime,
    pub slice: &'a [u8],
    pub read: usize,
    pub write_range: (usize, usize),
    pub ring_length: usize,
    #[allow(dead_code)]
    ring: &'a mut Ring,
}

impl BufferView<'_> {
    pub fn backlog(&self) -> usize {
        (self.write_range.0 + self.ring_length - 1 - self.read) % self.ring_length
    }
}

impl Ring {
    pub fn next_with_timeout(&mut self, duration: &std::time::Duration) -> Option<BufferView> {
        let (system_time, slice, read, write_range, ring_length) = {
            let start = std::time::Instant::now();
            // unwrap: mutex is not poisonned
            let mut shared = self.context.shared.lock().unwrap();
            loop {
                shared.read = (shared.read + 1) % shared.buffers.len();
                while (shared.write_range.1 + shared.buffers.len() - 1 - shared.read)
                    % shared.buffers.len()
                    < shared.transfer_statuses.len()
                {
                    let ellapsed = std::time::Instant::now() - start;
                    if ellapsed >= *duration {
                        return None;
                    }
                    // unwrap: shared_condvar always used with the same mutex (shared)
                    shared = self
                        .context
                        .shared_condvar
                        .wait_timeout(shared, *duration - ellapsed)
                        .unwrap()
                        .0;
                }
                if shared.buffers[shared.read].length > 0 {
                    break;
                }
            }
            (
                shared.buffers[shared.read].system_time,
                // unsafe: data validity guaranteed by read / write_range in shared
                unsafe {
                    std::slice::from_raw_parts(
                        shared.buffers[shared.read].data.as_ptr(),
                        shared.buffers[shared.read].length,
                    )
                },
                shared.read,
                shared.write_range,
                shared.buffers.len(),
            )
        };
        Some(BufferView {
            system_time,
            slice,
            read,
            write_range,
            ring_length,
            ring: self,
        })
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        let mut dealloc_buffers = true;
        let before_dealloc_transfers = std::time::Instant::now();
        {
            // unwrap: mutex is not poisonned
            let mut shared = self.context.shared.lock().unwrap();
            // unsafe: transfer is allocated
            let _ = unsafe { libusb1_sys::libusb_cancel_transfer(self.transfers[0].as_ptr()) };
            #[cfg(target_os = "macos")]
            {
                for index in 0..self.transfers.len() {
                    shared.transfer_statuses[index] = TransferStatus::Cancelling;
                }
            }
        }
        loop {
            let mut deallocated_transfers: usize = 0;
            {
                // unwrap: mutex is not poisonned
                let mut shared = self.context.shared.lock().unwrap();
                for index in 0..self.transfers.len() {
                    match shared.transfer_statuses[index] {
                        TransferStatus::Active => {
                            if unsafe {
                                libusb1_sys::libusb_cancel_transfer(self.transfers[index].as_ptr())
                            } == 0
                            {
                                shared.transfer_statuses[index] = TransferStatus::Cancelling;
                            } else {
                                shared.transfer_statuses[index] = TransferStatus::Complete;
                            }
                        }
                        TransferStatus::Complete => {
                            // unsafe: transfer is allocated and user_data is an allocated *mut TransferContext
                            let _transfer_context = unsafe {
                                Box::from_raw(
                                    (self.transfers[index].as_mut()).user_data
                                        as *mut TransferContext,
                                )
                            };
                            // unsafe: transfer is allocated
                            let _ = unsafe {
                                libusb1_sys::libusb_free_transfer(self.transfers[index].as_ptr())
                            };
                            shared.transfer_statuses[index] = TransferStatus::Deallocated;
                            deallocated_transfers += 1;
                        }
                        TransferStatus::Cancelling => (),
                        TransferStatus::Deallocated => {
                            deallocated_transfers += 1;
                        }
                    }
                }
            }
            if deallocated_transfers == self.transfers.len() {
                break;
            }
            // give up if the transfers are not freed after one second (better leak memory that loop forever)
            if std::time::Instant::now() - before_dealloc_transfers
                > std::time::Duration::from_secs(1)
            {
                dealloc_buffers = false;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if dealloc_buffers {
            // unwrap: mutex is not poisonned
            let shared = self.context.shared.lock().unwrap();
            for buffer in shared.buffers.iter() {
                if buffer.dma {
                    // unsafe: buffer was allocated by libusb with 'capacity' bytes
                    unsafe {
                        libusb_dev_mem_free(
                            self.handle.as_raw(),
                            buffer.data.as_ptr() as *mut libc::c_uchar,
                            buffer.capacity as libc::ssize_t,
                        );
                    };
                } else {
                    // unsafe: buffer was allocated by alloc with 'Layout {capacity, 1}'
                    unsafe {
                        std::alloc::dealloc(
                            buffer.data.as_ptr(),
                            std::alloc::Layout::from_size_align_unchecked(buffer.capacity, 1),
                        );
                    }
                }
            }
        }
    }
}
