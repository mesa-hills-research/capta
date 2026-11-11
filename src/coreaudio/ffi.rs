//! The parts of Core Foundation, Core Audio and Audio Toolbox that Capta calls, declared by hand
//! from the macOS SDK headers, with safe wrappers around them.
//!
//! A [`Queue`] calls back on a thread of Audio Toolbox's own, where Capta's callback takes no
//! lock, allocates nothing and calls nothing but the queue. Every `unsafe` block is listed and
//! argued in docs/why-this-is-safe.md.

use std::ffi::{c_char, c_void};
use std::mem::offset_of;
use std::ptr::{self, NonNull};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::native::{MAX_DEVICES, Ring, Timeline};

pub(crate) type OsStatus = i32;
pub(crate) type AudioObjectId = u32;
type CfIndex = isize;
type CfStringRef = *const c_void;
type CfRunLoopRef = *mut c_void;
type AudioQueueRef = *mut c_void;

const SYSTEM_OBJECT: AudioObjectId = 1;
const UNKNOWN_OBJECT: AudioObjectId = 0;

/// Four-character codes, as the headers spell them.
const fn code(text: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*text)
}

const SCOPE_GLOBAL: u32 = code(b"glob");
const SCOPE_INPUT: u32 = code(b"inpt");
const ELEMENT_MAIN: u32 = 0;
const HARDWARE_DEVICES: u32 = code(b"dev#");
const HARDWARE_DEFAULT_INPUT_DEVICE: u32 = code(b"dIn ");
const DEVICE_UID: u32 = code(b"uid ");
const OBJECT_NAME: u32 = code(b"lnam");
const DEVICE_STREAMS: u32 = code(b"stm#");
const DEVICE_IS_ALIVE: u32 = code(b"livn");
const DEVICE_NOMINAL_SAMPLE_RATE: u32 = code(b"nsrt");
const QUEUE_CURRENT_DEVICE: u32 = code(b"aqcd");
const FORMAT_LINEAR_PCM: u32 = code(b"lpcm");
/// `kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked`, in the machine's byte order.
const FORMAT_FLAGS_FLOAT_PACKED: u32 = 1 | 8;
/// `kAudioTimeStampSampleTimeValid`.
const SAMPLE_TIME_VALID: u32 = 1;
const STRING_ENCODING_UTF8: u32 = 0x0800_0100;
/// The longest string read, in bytes. Real ones are far smaller.
const MAX_STRING_BYTES: usize = 64 * 1024;

/// Why a call failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// The system's status code.
    Status(OsStatus),
    /// The system answered with something of the wrong size or kind.
    Malformed(&'static str),
}

fn check(status: OsStatus) -> Result<(), Failure> {
    if status == 0 {
        Ok(())
    } else {
        Err(Failure::Status(status))
    }
}

#[repr(C)]
struct PropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

/// `AudioStreamBasicDescription`.
#[repr(C)]
struct StreamDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

/// `SMPTETime`, inside a time stamp.
#[repr(C)]
struct SmpteTime {
    subframes: i16,
    subframe_divisor: i16,
    counter: u32,
    kind: u32,
    flags: u32,
    hours: i16,
    minutes: i16,
    seconds: i16,
    frames: i16,
}

/// `AudioTimeStamp`. Capta reads only its flags and the sample time they vouch for, through
/// [`read_start`].
#[repr(C)]
struct TimeStamp {
    sample_time: f64,
    host_time: u64,
    rate_scalar: f64,
    word_clock_time: u64,
    smpte_time: SmpteTime,
    flags: u32,
    reserved: u32,
}

/// `AudioQueueBuffer`. Capta reads only the fields the queue sets for a PCM recording, through
/// [`read_filled`], and never the struct as a whole.
#[repr(C)]
struct QueueBuffer {
    capacity: u32,
    data: *mut c_void,
    byte_size: u32,
    user_data: *mut c_void,
    packet_description_capacity: u32,
    packet_descriptions: *mut c_void,
    packet_description_count: u32,
}

/// `AudioQueueInputCallback`.
type InputCallback = extern "C" fn(
    *mut c_void,
    AudioQueueRef,
    *mut QueueBuffer,
    *const TimeStamp,
    u32,
    *const c_void,
);

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(object: *const c_void);
    fn CFStringCreateWithBytes(
        allocator: *const c_void,
        bytes: *const u8,
        length: CfIndex,
        encoding: u32,
        is_external_representation: u8,
    ) -> CfStringRef;
    fn CFStringGetLength(string: CfStringRef) -> CfIndex;
    fn CFStringGetMaximumSizeForEncoding(length: CfIndex, encoding: u32) -> CfIndex;
    fn CFStringGetCString(
        string: CfStringRef,
        buffer: *mut c_char,
        size: CfIndex,
        encoding: u32,
    ) -> u8;
}

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyDataSize(
        object: AudioObjectId,
        address: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
    ) -> OsStatus;
    fn AudioObjectGetPropertyData(
        object: AudioObjectId,
        address: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
        data: *mut c_void,
    ) -> OsStatus;
}

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioQueueNewInput(
        format: *const StreamDescription,
        callback: InputCallback,
        user_data: *mut c_void,
        run_loop: CfRunLoopRef,
        run_loop_mode: CfStringRef,
        flags: u32,
        queue: *mut AudioQueueRef,
    ) -> OsStatus;
    fn AudioQueueSetProperty(
        queue: AudioQueueRef,
        property: u32,
        data: *const c_void,
        size: u32,
    ) -> OsStatus;
    fn AudioQueueAllocateBuffer(
        queue: AudioQueueRef,
        size: u32,
        buffer: *mut *mut QueueBuffer,
    ) -> OsStatus;
    fn AudioQueueEnqueueBuffer(
        queue: AudioQueueRef,
        buffer: *mut QueueBuffer,
        descriptions: u32,
        description: *const c_void,
    ) -> OsStatus;
    fn AudioQueueStart(queue: AudioQueueRef, start_time: *const TimeStamp) -> OsStatus;
    fn AudioQueueDispose(queue: AudioQueueRef, immediate: u8) -> OsStatus;
}

/// A property value that any bit pattern of its size is valid for.
trait Plain: Copy {
    const ZERO: Self;
}

impl Plain for u32 {
    const ZERO: u32 = 0;
}

impl Plain for f64 {
    const ZERO: f64 = 0.0;
}

impl Plain for CfStringRef {
    const ZERO: CfStringRef = ptr::null();
}

fn address(selector: u32, scope: u32) -> PropertyAddress {
    PropertyAddress {
        selector,
        scope,
        element: ELEMENT_MAIN,
    }
}

/// Reads a property that holds one `T`.
fn property<T: Plain>(object: AudioObjectId, selector: u32, scope: u32) -> Result<T, Failure> {
    let address = address(selector, scope);
    let mut value = T::ZERO;
    let expected = size_of::<T>() as u32;
    let mut size = expected;
    // SAFETY: `address` is a valid property address, there's no qualifier, and `value` is a
    // `T` with `size` bytes of room. The call writes at most `size` bytes and sets `size` to
    // what it wrote.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            &address,
            0,
            ptr::null(),
            &mut size,
            (&raw mut value).cast(),
        )
    };
    check(status)?;
    if size != expected {
        return Err(Failure::Malformed("a property of an unexpected size"));
    }
    Ok(value)
}

/// The size of a property's value, in bytes.
fn property_size(object: AudioObjectId, selector: u32, scope: u32) -> Result<u32, Failure> {
    let address = address(selector, scope);
    let mut size = 0;
    // SAFETY: `address` is a valid property address, there's no qualifier, and `size` is a
    // valid place for the size.
    let status =
        unsafe { AudioObjectGetPropertyDataSize(object, &address, 0, ptr::null(), &mut size) };
    check(status)?;
    Ok(size)
}

/// The system's audio devices.
pub(crate) fn devices() -> Result<Vec<AudioObjectId>, Failure> {
    let item = size_of::<AudioObjectId>();
    let size = property_size(SYSTEM_OBJECT, HARDWARE_DEVICES, SCOPE_GLOBAL)?;
    let count = size as usize / item;
    if !(size as usize).is_multiple_of(item) || count > MAX_DEVICES {
        return Err(Failure::Malformed("a device list of an unexpected size"));
    }
    let mut devices = vec![UNKNOWN_OBJECT; count];
    let mut size = (count * item) as u32;
    let address = address(HARDWARE_DEVICES, SCOPE_GLOBAL);
    // SAFETY: `address` is a valid property address, there's no qualifier, and `devices` has
    // `size` bytes of room for device ids, of which the call writes at most `size` and says how
    // many.
    let status = unsafe {
        AudioObjectGetPropertyData(
            SYSTEM_OBJECT,
            &address,
            0,
            ptr::null(),
            &mut size,
            devices.as_mut_ptr().cast(),
        )
    };
    check(status)?;
    if !(size as usize).is_multiple_of(item) || size as usize > count * item {
        return Err(Failure::Malformed("a device list of an unexpected size"));
    }
    devices.truncate(size as usize / item);
    Ok(devices)
}

/// The default input device, or none when the system has none.
pub(crate) fn default_input() -> Result<Option<AudioObjectId>, Failure> {
    let device: u32 = property(SYSTEM_OBJECT, HARDWARE_DEFAULT_INPUT_DEVICE, SCOPE_GLOBAL)?;
    Ok((device != UNKNOWN_OBJECT).then_some(device))
}

/// Whether the device has an input stream.
pub(crate) fn has_input(device: AudioObjectId) -> Result<bool, Failure> {
    Ok(property_size(device, DEVICE_STREAMS, SCOPE_INPUT)? > 0)
}

/// The device's persistent unique id.
pub(crate) fn uid(device: AudioObjectId) -> Result<String, Failure> {
    let uid = copied_string(device, DEVICE_UID)?;
    uid.text()
        .ok_or(Failure::Malformed("a device UID that isn't valid UTF-8"))
}

/// The device's name, or none when it has none.
pub(crate) fn name(device: AudioObjectId) -> Result<Option<String>, Failure> {
    Ok(copied_string(device, OBJECT_NAME)?.text())
}

/// The rate the device runs at.
pub(crate) fn nominal_rate(device: AudioObjectId) -> Result<f64, Failure> {
    property(device, DEVICE_NOMINAL_SAMPLE_RATE, SCOPE_GLOBAL)
}

/// Whether the device is still there.
pub(crate) fn is_alive(device: AudioObjectId) -> Result<bool, Failure> {
    Ok(property::<u32>(device, DEVICE_IS_ALIVE, SCOPE_GLOBAL)? != 0)
}

/// Reads a property that hands the caller a string to release.
fn copied_string(device: AudioObjectId, selector: u32) -> Result<CfString, Failure> {
    let string: CfStringRef = property(device, selector, SCOPE_GLOBAL)?;
    let string = NonNull::new(string.cast_mut()).ok_or(Failure::Malformed("a null string"))?;
    // SAFETY: the UID and name properties hand over a CFString the caller releases, as
    // AudioHardware.h says of every CFType property.
    Ok(unsafe { CfString::from_owned(string) })
}

/// A CFString this code holds a reference to, released when dropped.
pub(crate) struct CfString(NonNull<c_void>);

impl CfString {
    pub(crate) fn new(text: &str) -> Result<CfString, Failure> {
        let length =
            CfIndex::try_from(text.len()).map_err(|_| Failure::Malformed("a string too long"))?;
        // SAFETY: `text` is `length` bytes of UTF-8, the null allocator is the default one, and
        // the call copies the bytes.
        let string = unsafe {
            CFStringCreateWithBytes(ptr::null(), text.as_ptr(), length, STRING_ENCODING_UTF8, 0)
        };
        let string = NonNull::new(string.cast_mut())
            .ok_or(Failure::Malformed("a string the system couldn't make"))?;
        // SAFETY: CFStringCreateWithBytes follows the create rule, so the caller owns the
        // string it returns.
        Ok(unsafe { CfString::from_owned(string) })
    }

    /// Takes over a reference to a CFString.
    ///
    /// # Safety
    ///
    /// `string` is a CFString the caller owns a reference to and hands over.
    unsafe fn from_owned(string: NonNull<c_void>) -> CfString {
        CfString(string)
    }

    fn as_ptr(&self) -> CfStringRef {
        self.0.as_ptr().cast_const()
    }

    /// The string as UTF-8, or none when it's longer than [`MAX_STRING_BYTES`].
    pub(crate) fn text(&self) -> Option<String> {
        // SAFETY: `self` holds a reference to a live CFString.
        let length = unsafe { CFStringGetLength(self.as_ptr()) };
        // SAFETY: a pure computation on a length, which returns kCFNotFound on overflow.
        let most = unsafe { CFStringGetMaximumSizeForEncoding(length, STRING_ENCODING_UTF8) };
        let size = usize::try_from(most).ok()?.checked_add(1)?;
        if size > MAX_STRING_BYTES {
            return None;
        }
        let mut bytes = vec![0u8; size];
        // SAFETY: `self` is a live CFString, and `bytes` has room for `size` bytes, which the
        // call fills with the string and a terminating NUL, or fails.
        let copied = unsafe {
            CFStringGetCString(
                self.as_ptr(),
                bytes.as_mut_ptr().cast(),
                size as CfIndex,
                STRING_ENCODING_UTF8,
            )
        };
        if copied == 0 {
            return None;
        }
        let end = bytes.iter().position(|&byte| byte == 0)?;
        bytes.truncate(end);
        String::from_utf8(bytes).ok()
    }
}

impl Drop for CfString {
    fn drop(&mut self) {
        // SAFETY: `self` owns one reference, given back once here.
        unsafe { CFRelease(self.as_ptr()) };
    }
}

/// What a queue's callback shares with the capture thread. The callback pushes samples and
/// records how recording went, and the capture thread pops them, all through atomics.
struct Inbox {
    ring: Ring,
    /// Where the next buffer should start, as the bits of an `f64`, or NaN before the first.
    next_start: AtomicU64,
    tolerance: f64,
    /// A [`Gathered`], once it isn't `Fine`, encoded by [`Gathered::bits`].
    state: AtomicU64,
    closing: AtomicBool,
    /// Whether a callback is taking a buffer, so that only one pushes to the ring at a time.
    taking: AtomicBool,
}

/// How recording went since it started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Gathered {
    Fine,
    /// Audio was lost before the last buffer, by the queue.
    Gap,
    /// The capture thread fell behind and the ring filled.
    Full,
    /// A buffer held a part of a frame.
    Malformed,
    /// A buffer couldn't go back to the queue.
    Failed(OsStatus),
    /// Two callbacks ran at once, so the order of their buffers is unknown.
    Overlap,
}

impl Gathered {
    fn bits(self) -> u64 {
        match self {
            Gathered::Fine => 0,
            Gathered::Gap => 1,
            Gathered::Full => 2,
            Gathered::Malformed => 3,
            Gathered::Overlap => 5,
            Gathered::Failed(status) => 4 << 32 | u64::from(status.cast_unsigned()),
        }
    }

    fn from_bits(bits: u64) -> Gathered {
        match bits >> 32 {
            4 => Gathered::Failed((bits as u32).cast_signed()),
            _ => match bits {
                1 => Gathered::Gap,
                2 => Gathered::Full,
                3 => Gathered::Malformed,
                5 => Gathered::Overlap,
                _ => Gathered::Fine,
            },
        }
    }
}

impl Inbox {
    fn state(&self) -> Gathered {
        Gathered::from_bits(self.state.load(Ordering::Acquire))
    }

    /// Records how recording went, unless something went wrong before.
    fn fail(&self, how: Gathered) {
        let _ = self.state.compare_exchange(
            Gathered::Fine.bits(),
            how.bits(),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    /// Takes a buffer, unless another callback is taking one, which ends the recording. So the
    /// ring has one producer at a time, whatever threads the queue calls back on.
    fn take(&self, bytes: &[u8], start: Option<f64>) {
        if self.taking.swap(true, Ordering::Acquire) {
            self.fail(Gathered::Overlap);
            return;
        }
        self.receive(bytes, start);
        self.taking.store(false, Ordering::Release);
    }

    /// Takes a buffer's bytes and the sample time they start at, when the queue gave one.
    fn receive(&self, bytes: &[u8], start: Option<f64>) {
        if self.state() != Gathered::Fine {
            return;
        }
        if !bytes.len().is_multiple_of(4) {
            self.fail(Gathered::Malformed);
            return;
        }
        let (samples, _) = bytes.as_chunks::<4>();
        if let Some(start) = start {
            let next = f64::from_bits(self.next_start.load(Ordering::Relaxed));
            let mut timeline =
                Timeline::resume(Some(next).filter(|next| !next.is_nan()), self.tolerance);
            let follows = timeline.follows(start, samples.len());
            let next = timeline.next().unwrap_or(f64::NAN);
            self.next_start.store(next.to_bits(), Ordering::Relaxed);
            if !follows {
                self.fail(Gathered::Gap);
                return;
            }
        }
        if !self
            .ring
            .push(samples.iter().map(|&sample| f32::from_ne_bytes(sample)))
        {
            self.fail(Gathered::Full);
        }
    }
}

/// The fields of a filled buffer that the queue sets for a PCM recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Filled {
    capacity: u32,
    data: *mut c_void,
    byte_size: u32,
}

/// Reads the capacity, data pointer and byte size of `buffer`, and none of its other fields,
/// which the queue may leave unwritten for a buffer without packet descriptions.
///
/// # Safety
///
/// `buffer` points to an `AudioQueueBuffer` whose `mAudioDataBytesCapacity`, `mAudioData` and
/// `mAudioDataByteSize` are initialized and not written during the call.
unsafe fn read_filled(buffer: NonNull<QueueBuffer>) -> Filled {
    let base = buffer.as_ptr().cast::<u8>();
    let capacity = base.wrapping_add(offset_of!(QueueBuffer, capacity));
    let data = base.wrapping_add(offset_of!(QueueBuffer, data));
    let byte_size = base.wrapping_add(offset_of!(QueueBuffer, byte_size));
    Filled {
        // SAFETY: by the contract the field is initialized. Its offset within the `repr(C)`
        // struct keeps it inside the buffer and aligned as a `u32`.
        capacity: unsafe { capacity.cast::<u32>().read() },
        // SAFETY: as above, for the pointer field.
        data: unsafe { data.cast::<*mut c_void>().read() },
        // SAFETY: as above, for the byte size.
        byte_size: unsafe { byte_size.cast::<u32>().read() },
    }
}

/// Reads the sample time a time stamp holds, when its flags say it holds one, and nothing
/// else of it.
///
/// # Safety
///
/// `start` points to an `AudioTimeStamp` whose `mFlags` is initialized, and whose `mSampleTime`
/// is initialized when `mFlags` says it's valid, neither written during the call.
unsafe fn read_start(start: NonNull<TimeStamp>) -> Option<f64> {
    let base = start.as_ptr().cast::<u8>();
    let flags = base.wrapping_add(offset_of!(TimeStamp, flags));
    let sample_time = base.wrapping_add(offset_of!(TimeStamp, sample_time));
    // SAFETY: by the contract the flags are initialized. The offset within the `repr(C)` struct
    // keeps them inside it and aligned as a `u32`.
    let flags = unsafe { flags.cast::<u32>().read() };
    (flags & SAMPLE_TIME_VALID != 0).then(|| {
        // SAFETY: the flags say the sample time is set, at an offset aligned for an `f64`.
        unsafe { sample_time.cast::<f64>().read() }
    })
}

/// What a recording queue calls on one of its own threads with each filled buffer. It takes no
/// lock, allocates nothing and calls nothing but `AudioQueueEnqueueBuffer`.
extern "C" fn on_buffer(
    user: *mut c_void,
    queue: AudioQueueRef,
    buffer: *mut QueueBuffer,
    start: *const TimeStamp,
    _descriptions: u32,
    _description: *const c_void,
) {
    // SAFETY: `user` is the inbox of the `Queue` that made this queue. The `Queue` holds it
    // until AudioQueueDispose has returned on the capture thread, after which no callback runs.
    // It's `Sync` and changes only through atomics, so this shared reference is valid on this
    // thread while the capture thread holds another.
    let inbox = unsafe { &*user.cast::<Inbox>() };
    if inbox.closing.load(Ordering::Acquire) {
        return;
    }
    let Some(buffer) = NonNull::new(buffer) else {
        return;
    };
    // SAFETY: the queue hands the callback one of its own buffers, allocated by
    // AudioQueueAllocateBuffer, which sets its capacity and data pointer and, once filled, its
    // byte size. Nothing writes them until the buffer is enqueued again below.
    let filled = unsafe { read_filled(buffer) };
    let start = match NonNull::new(start.cast_mut()) {
        // SAFETY: when not null, `start` points to the buffer's time stamp, valid during the
        // callback, whose flags say which of its fields are set.
        Some(start) => unsafe { read_start(start) },
        None => None,
    };
    match NonNull::new(filled.data.cast::<u8>()) {
        Some(data) if filled.byte_size <= filled.capacity => {
            // SAFETY: the buffer's data holds `capacity` bytes, of which the queue filled the
            // first `byte_size`, and they stay unchanged until the buffer is enqueued again.
            let bytes =
                unsafe { std::slice::from_raw_parts(data.as_ptr(), filled.byte_size as usize) };
            inbox.take(bytes, start);
        }
        None if filled.byte_size == 0 => {}
        _ => inbox.fail(Gathered::Malformed),
    }
    // SAFETY: `queue` and `buffer` are the ones the queue passed. An input queue takes a buffer
    // back with no packet descriptions.
    let status = unsafe { AudioQueueEnqueueBuffer(queue, buffer.as_ptr(), 0, ptr::null()) };
    if status != 0 {
        inbox.fail(Gathered::Failed(status));
    }
}

/// A recording Audio Queue. Dropping it disposes of the queue.
pub(crate) struct Queue {
    queue: NonNull<c_void>,
    // Freed after the queue is disposed of, since fields drop after `drop` runs. An `Arc`, not a
    // `Box`, since the callback reads it through a shared pointer on another thread while the
    // `Queue` moves.
    inbox: Arc<Inbox>,
}

impl Queue {
    /// A queue that records mono floats at `rate`, holds up to `capacity` samples for the
    /// capture thread, and lets buffer start times drift by up to `tolerance` frames.
    pub(crate) fn new_input(rate: u32, capacity: usize, tolerance: f64) -> Result<Queue, Failure> {
        let inbox = Arc::new(Inbox {
            ring: Ring::new(capacity),
            next_start: AtomicU64::new(f64::NAN.to_bits()),
            tolerance,
            state: AtomicU64::new(Gathered::Fine.bits()),
            closing: AtomicBool::new(false),
            taking: AtomicBool::new(false),
        });
        let format = StreamDescription {
            sample_rate: f64::from(rate),
            format_id: FORMAT_LINEAR_PCM,
            format_flags: FORMAT_FLAGS_FLOAT_PACKED,
            bytes_per_packet: 4,
            frames_per_packet: 1,
            bytes_per_frame: 4,
            channels_per_frame: 1,
            bits_per_channel: 32,
            reserved: 0,
        };
        let user = Arc::as_ptr(&inbox).cast_mut().cast::<c_void>();
        let mut queue = ptr::null_mut();
        // SAFETY: `format` is a valid linear PCM description, `on_buffer` matches the callback
        // type, `user` is the inbox that outlives the queue, a null run loop has the queue call
        // back on its own thread, and `queue` is a valid place for the result.
        let status = unsafe {
            AudioQueueNewInput(
                &format,
                on_buffer,
                user,
                ptr::null_mut(),
                ptr::null(),
                0,
                &mut queue,
            )
        };
        check(status)?;
        let queue = NonNull::new(queue).ok_or(Failure::Malformed("a null queue"))?;
        Ok(Queue { queue, inbox })
    }

    /// Records from the device whose UID is `uid` instead of the default.
    pub(crate) fn set_device(&self, uid: &CfString) -> Result<(), Failure> {
        let uid = uid.as_ptr();
        // SAFETY: the queue is live, and the property takes a CFString, passed as a pointer to
        // it with its size. The queue retains the string.
        let status = unsafe {
            AudioQueueSetProperty(
                self.queue.as_ptr(),
                QUEUE_CURRENT_DEVICE,
                (&raw const uid).cast(),
                size_of::<CfStringRef>() as u32,
            )
        };
        check(status)
    }

    /// Gives the queue `count` buffers of `bytes` each to record into.
    pub(crate) fn add_buffers(&self, count: usize, bytes: u32) -> Result<(), Failure> {
        for _ in 0..count {
            let mut buffer = ptr::null_mut();
            // SAFETY: the queue is live and `buffer` a valid place for the new buffer, which the
            // queue owns and frees when it's disposed of.
            check(unsafe { AudioQueueAllocateBuffer(self.queue.as_ptr(), bytes, &mut buffer) })?;
            if buffer.is_null() {
                return Err(Failure::Malformed("a null buffer"));
            }
            // SAFETY: `buffer` is the queue's own, not enqueued yet, and an input queue takes no
            // packet descriptions.
            let status =
                unsafe { AudioQueueEnqueueBuffer(self.queue.as_ptr(), buffer, 0, ptr::null()) };
            check(status)?;
        }
        Ok(())
    }

    pub(crate) fn start(&self) -> Result<(), Failure> {
        // SAFETY: the queue is live, and a null start time means as soon as possible.
        check(unsafe { AudioQueueStart(self.queue.as_ptr(), ptr::null()) })
    }

    /// Moves the samples gathered since the last call to `out`, and says how recording went.
    /// The state is read first, so the samples include every one before a gap.
    pub(crate) fn drain(&self, out: &mut Vec<f32>) -> Gathered {
        let state = self.inbox.state();
        self.inbox.ring.pop_into(out);
        state
    }
}

impl Drop for Queue {
    fn drop(&mut self) {
        self.inbox.closing.store(true, Ordering::Release);
        // SAFETY: the queue is live and disposed of once, here, on the capture thread, which
        // isn't the queue's callback thread. Disposing immediately means no callback runs after
        // this returns, so the inbox can go after it.
        unsafe { AudioQueueDispose(self.queue.as_ptr(), 1) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_match_the_headers() {
        use std::mem::offset_of as at;
        assert_eq!(size_of::<PropertyAddress>(), 12);
        assert_eq!(
            [
                at!(PropertyAddress, selector),
                at!(PropertyAddress, scope),
                at!(PropertyAddress, element)
            ],
            [0, 4, 8]
        );
        assert_eq!(size_of::<StreamDescription>(), 40);
        assert_eq!(
            [
                at!(StreamDescription, sample_rate),
                at!(StreamDescription, format_id),
                at!(StreamDescription, format_flags),
                at!(StreamDescription, bytes_per_packet),
                at!(StreamDescription, frames_per_packet),
                at!(StreamDescription, bytes_per_frame),
                at!(StreamDescription, channels_per_frame),
                at!(StreamDescription, bits_per_channel),
                at!(StreamDescription, reserved),
            ],
            [0, 8, 12, 16, 20, 24, 28, 32, 36]
        );
        assert_eq!(size_of::<SmpteTime>(), 24);
        assert_eq!(
            [
                at!(SmpteTime, subframes),
                at!(SmpteTime, subframe_divisor),
                at!(SmpteTime, counter),
                at!(SmpteTime, kind),
                at!(SmpteTime, flags),
                at!(SmpteTime, hours),
                at!(SmpteTime, minutes),
                at!(SmpteTime, seconds),
                at!(SmpteTime, frames),
            ],
            [0, 2, 4, 8, 12, 16, 18, 20, 22]
        );
        assert_eq!(size_of::<TimeStamp>(), 64);
        assert_eq!(
            [
                at!(TimeStamp, sample_time),
                at!(TimeStamp, host_time),
                at!(TimeStamp, rate_scalar),
                at!(TimeStamp, word_clock_time),
                at!(TimeStamp, smpte_time),
                at!(TimeStamp, flags),
                at!(TimeStamp, reserved),
            ],
            [0, 8, 16, 24, 32, 56, 60]
        );
        assert_eq!(size_of::<QueueBuffer>(), 56);
        assert_eq!(
            [
                at!(QueueBuffer, capacity),
                at!(QueueBuffer, data),
                at!(QueueBuffer, byte_size),
                at!(QueueBuffer, user_data),
                at!(QueueBuffer, packet_description_capacity),
                at!(QueueBuffer, packet_descriptions),
                at!(QueueBuffer, packet_description_count),
            ],
            [0, 8, 16, 24, 32, 40, 48]
        );
        assert_eq!(code(b"lpcm"), 0x6c70_636d);
    }

    #[test]
    fn strings_go_there_and_back() {
        let string = CfString::new("Fixture Microphone, 2 µs").unwrap();
        assert_eq!(string.text().unwrap(), "Fixture Microphone, 2 µs");
    }

    fn inbox(capacity: usize) -> Inbox {
        Inbox {
            ring: Ring::new(capacity),
            next_start: AtomicU64::new(f64::NAN.to_bits()),
            tolerance: 1.0,
            state: AtomicU64::new(Gathered::Fine.bits()),
            closing: AtomicBool::new(false),
            taking: AtomicBool::new(false),
        }
    }

    fn bytes(samples: &[f32]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_ne_bytes()).collect()
    }

    #[test]
    fn a_gap_stops_the_gathering() {
        let inbox = inbox(64);
        let pair = bytes(&[0.5, 0.25]);
        inbox.receive(&pair, Some(0.0));
        inbox.receive(&pair, Some(2.0));
        inbox.receive(&pair, Some(40.0));
        inbox.receive(&pair, Some(42.0));
        let mut out = Vec::new();
        inbox.ring.pop_into(&mut out);
        assert_eq!(out, [0.5, 0.25, 0.5, 0.25]);
        assert_eq!(inbox.state(), Gathered::Gap);
    }

    #[test]
    fn a_full_ring_and_a_partial_frame_stop_the_gathering() {
        let full = inbox(3);
        full.receive(&bytes(&[0.5, 0.25]), None);
        full.receive(&bytes(&[0.5, 0.25]), None);
        assert_eq!(full.state(), Gathered::Full);
        let partial = inbox(8);
        partial.receive(&[0, 0, 0], None);
        assert_eq!(partial.state(), Gathered::Malformed);
    }

    #[test]
    fn a_second_callback_at_once_ends_the_recording() {
        let inbox = inbox(64);
        inbox.take(&bytes(&[0.5]), None);
        // A callback is still taking its buffer when another comes.
        inbox.taking.store(true, Ordering::SeqCst);
        inbox.take(&bytes(&[0.25]), None);
        assert_eq!(inbox.state(), Gathered::Overlap);
        let mut out = Vec::new();
        inbox.ring.pop_into(&mut out);
        assert_eq!(out, [0.5]);
    }

    #[test]
    fn a_filled_buffer_is_read_without_its_unset_fields() {
        // Only the three fields the queue sets are written. Under Miri, reading any other
        // field, as a whole-struct read does, fails the test.
        let mut buffer = std::mem::MaybeUninit::<QueueBuffer>::uninit();
        let mut audio = [0u8; 8];
        // Every pointer comes from this one, so none invalidates another.
        let raw = buffer.as_mut_ptr();
        let base = raw.cast::<u8>();
        let capacity = base.wrapping_add(offset_of!(QueueBuffer, capacity));
        let data = base.wrapping_add(offset_of!(QueueBuffer, data));
        let byte_size = base.wrapping_add(offset_of!(QueueBuffer, byte_size));
        // SAFETY: each pointer is a field of `buffer`'s storage, aligned for its type.
        unsafe { capacity.cast::<u32>().write(8) };
        // SAFETY: as above.
        unsafe { data.cast::<*mut c_void>().write(audio.as_mut_ptr().cast()) };
        // SAFETY: as above.
        unsafe { byte_size.cast::<u32>().write(4) };
        let buffer = NonNull::new(raw).unwrap();
        // SAFETY: the three fields are initialized and nothing writes them during the read.
        let filled = unsafe { read_filled(buffer) };
        assert_eq!(
            filled,
            Filled {
                capacity: 8,
                data: audio.as_mut_ptr().cast(),
                byte_size: 4,
            }
        );
    }

    #[test]
    fn a_time_stamp_is_read_only_as_far_as_its_flags_say() {
        let mut stamp = std::mem::MaybeUninit::<TimeStamp>::uninit();
        // Every pointer comes from this one, so none invalidates another.
        let raw = stamp.as_mut_ptr();
        let base = raw.cast::<u8>();
        let flags = base.wrapping_add(offset_of!(TimeStamp, flags));
        let sample_time = base.wrapping_add(offset_of!(TimeStamp, sample_time));
        // SAFETY: the pointer is the flags field of `stamp`'s storage, aligned for a `u32`.
        unsafe { flags.cast::<u32>().write(0) };
        let start = NonNull::new(raw).unwrap();
        // SAFETY: the flags are initialized and say nothing else is.
        assert_eq!(unsafe { read_start(start) }, None);
        // SAFETY: as above.
        unsafe { flags.cast::<u32>().write(SAMPLE_TIME_VALID | 2) };
        // SAFETY: the pointer is the sample time field, aligned for an `f64`.
        unsafe { sample_time.cast::<f64>().write(320.0) };
        // SAFETY: the flags and the sample time they vouch for are initialized.
        assert_eq!(unsafe { read_start(start) }, Some(320.0));
    }

    #[test]
    fn states_survive_their_encoding() {
        for state in [
            Gathered::Fine,
            Gathered::Gap,
            Gathered::Full,
            Gathered::Malformed,
            Gathered::Overlap,
            Gathered::Failed(-66681),
            Gathered::Failed(0x2168_6f67),
        ] {
            assert_eq!(Gathered::from_bits(state.bits()), state);
        }
    }

    #[test]
    fn the_system_lists_devices() {
        for device in devices().unwrap() {
            if has_input(device).unwrap_or(false) {
                assert!(!uid(device).unwrap().is_empty());
            }
        }
    }
}
