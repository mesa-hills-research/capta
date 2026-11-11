//! The parts of COM, the Windows audio APIs and Win32 that Capta calls, declared by hand from
//! the Windows SDK headers, with safe wrappers around them.
//!
//! Each wrapper of an interface owns one reference to its object and releases it when dropped.
//! Wrappers borrow the [`Apartment`] they were made in, so the compiler keeps every one of them
//! from outliving the thread's membership of COM's multithreaded apartment. None of them is
//! `Send`, so each stays on the thread that made it. Every `unsafe` block is listed and argued
//! in docs/why-this-is-safe.md.

use std::ffi::c_void;
use std::io;
use std::marker::PhantomData;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{self, NonNull};
use std::time::Duration;

use crate::native::byte_length;

/// A COM result code. Negative values are failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Hresult(pub i32);

impl Hresult {
    const fn from_bits(bits: u32) -> Hresult {
        Hresult(bits as i32)
    }

    fn ok(self) -> Result<(), Hresult> {
        if self.0 >= 0 { Ok(()) } else { Err(self) }
    }
}

pub(crate) const E_NOTFOUND: Hresult = Hresult::from_bits(0x8007_0490);
pub(crate) const E_ACCESSDENIED: Hresult = Hresult::from_bits(0x8007_0005);
pub(crate) const E_INVALIDARG: Hresult = Hresult::from_bits(0x8007_0057);
pub(crate) const E_OUTOFMEMORY: Hresult = Hresult::from_bits(0x8007_000E);
const E_POINTER: Hresult = Hresult::from_bits(0x8000_4003);
pub(crate) const AUDCLNT_E_DEVICE_INVALIDATED: Hresult = Hresult::from_bits(0x8889_0004);
pub(crate) const AUDCLNT_E_UNSUPPORTED_FORMAT: Hresult = Hresult::from_bits(0x8889_0008);
pub(crate) const AUDCLNT_E_DEVICE_IN_USE: Hresult = Hresult::from_bits(0x8889_000A);
pub(crate) const AUDCLNT_E_ENDPOINT_CREATE_FAILED: Hresult = Hresult::from_bits(0x8889_000F);
pub(crate) const AUDCLNT_E_SERVICE_NOT_RUNNING: Hresult = Hresult::from_bits(0x8889_0010);
pub(crate) const AUDCLNT_E_RESOURCES_INVALIDATED: Hresult = Hresult::from_bits(0x8889_0026);
const AUDCLNT_S_BUFFER_EMPTY: Hresult = Hresult::from_bits(0x0889_0001);

/// `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` and `AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY`. The
/// engine converts the device's format and rate to the client's.
const STREAM_FLAGS: u32 = 0x8000_0000 | 0x0800_0000;
/// `AUDCLNT_STREAMFLAGS_EVENTCALLBACK`, which Capta doesn't use.
#[cfg(test)]
const EVENT_CALLBACK: u32 = 0x0004_0000;
/// The engine buffer Capta asks for, 2 s in 100 ns units.
const ENGINE_BUFFER: i64 = 20_000_000;
/// `AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY`, audio was lost before this packet.
pub(crate) const DATA_DISCONTINUITY: u32 = 1;
/// `AUDCLNT_BUFFERFLAGS_SILENT`, the packet is silence whatever its bytes hold.
pub(crate) const SILENT: u32 = 2;

const COINIT_MULTITHREADED: u32 = 0;
const CLSCTX_ALL: u32 = 0x17;
const E_CAPTURE: i32 = 1;
const E_CONSOLE: i32 = 0;
const DEVICE_STATE_ACTIVE: u32 = 1;
const STGM_READ: u32 = 0;
const VT_LPWSTR: u16 = 31;
const AUDCLNT_SHAREMODE_SHARED: i32 = 0;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 0x102;
const INFINITE: u32 = u32::MAX;
/// The longest string read from the system, in UTF-16 units. Endpoint ids and names are far
/// shorter.
const MAX_UNITS: usize = 32 * 1024;

#[repr(C)]
#[derive(Clone, Copy)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

impl Guid {
    const fn from_u128(value: u128) -> Guid {
        Guid {
            data1: (value >> 96) as u32,
            data2: (value >> 80) as u16,
            data3: (value >> 64) as u16,
            data4: (value as u64).to_be_bytes(),
        }
    }
}

static CLSID_MM_DEVICE_ENUMERATOR: Guid = Guid::from_u128(0xbcde0395_e52f_467c_8e3d_c4579291692e);
static IID_IMM_DEVICE_ENUMERATOR: Guid = Guid::from_u128(0xa95664d2_9614_4f35_a746_de8db63617e6);
static IID_IAUDIO_CLIENT: Guid = Guid::from_u128(0x1cb9ad4c_dbfa_4c32_b178_c2f568a703b2);
static IID_IAUDIO_CAPTURE_CLIENT: Guid = Guid::from_u128(0xc8adbd64_e71e_48a0_a4de_185c395cd317);

#[repr(C)]
struct PropertyKey {
    fmtid: Guid,
    pid: u32,
}

/// `PKEY_Device_FriendlyName`, such as "Microphone (Fixture Audio)".
static PKEY_DEVICE_FRIENDLY_NAME: PropertyKey = PropertyKey {
    fmtid: Guid::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
    pid: 14,
};
/// `PKEY_Device_DeviceDesc`, such as "Microphone".
static PKEY_DEVICE_DESC: PropertyKey = PropertyKey {
    fmtid: Guid::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
    pid: 2,
};

/// `PROPVARIANT`, of which Capta reads only the type and a string pointer. The union is as large
/// and as aligned as the header's, 24 bytes in all on 64-bit Windows and 16 on 32-bit.
#[repr(C)]
struct PropVariant {
    vt: u16,
    reserved: [u16; 3],
    value: PropValue,
}

#[repr(C)]
#[derive(Clone, Copy)]
union PropValue {
    text: *mut u16,
    large: u64,
    blob: [usize; 2],
}

/// `WAVEFORMATEX`, packed as in mmreg.h.
#[repr(C, packed(1))]
#[derive(Clone, Copy)]
pub(crate) struct WaveFormatEx {
    format_tag: u16,
    channels: u16,
    samples_per_sec: u32,
    avg_bytes_per_sec: u32,
    block_align: u16,
    bits_per_sample: u16,
    size: u16,
}

/// How Capta initializes an audio client, in one place, so every attempt opens the same mode.
/// A shared-mode stream that the client polls, rather than one the engine drives with an event,
/// gets an engine buffer of the length asked for, and the periodicity must be 0.
#[derive(Clone, Copy)]
pub(crate) struct SharedCapture {
    format: WaveFormatEx,
    flags: u32,
    buffer: i64,
    periodicity: i64,
}

impl SharedCapture {
    /// One channel of 32-bit floats at `rate`, in a 2 s engine buffer.
    pub(crate) fn mono_f32(rate: u32) -> SharedCapture {
        SharedCapture {
            format: WaveFormatEx::mono_f32(rate),
            flags: STREAM_FLAGS,
            buffer: ENGINE_BUFFER,
            periodicity: 0,
        }
    }
}

impl WaveFormatEx {
    /// One channel of 32-bit floats at `rate`.
    fn mono_f32(rate: u32) -> WaveFormatEx {
        WaveFormatEx {
            format_tag: WAVE_FORMAT_IEEE_FLOAT,
            channels: 1,
            samples_per_sec: rate,
            avg_bytes_per_sec: rate.saturating_mul(4),
            block_align: 4,
            bits_per_sample: 32,
            size: 0,
        }
    }
}

type Method0 = unsafe extern "system" fn(*mut c_void) -> i32;

#[repr(C)]
struct UnknownVtbl {
    query_interface: usize,
    add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

/// A vtable, which begins with IUnknown's three methods. Each struct below lists its interface's
/// methods in the header's order up to the last one Capta calls. Methods it doesn't call are
/// `usize`, the size of a function pointer.
trait Vtable {
    fn unknown(&self) -> &UnknownVtbl;
}

/// `IMMDeviceEnumerator`.
#[repr(C)]
struct EnumeratorVtbl {
    unknown: UnknownVtbl,
    enum_audio_endpoints: unsafe extern "system" fn(*mut c_void, i32, u32, *mut *mut c_void) -> i32,
    get_default_audio_endpoint:
        unsafe extern "system" fn(*mut c_void, i32, i32, *mut *mut c_void) -> i32,
}

/// `IMMDeviceCollection`.
#[repr(C)]
struct CollectionVtbl {
    unknown: UnknownVtbl,
    get_count: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32,
    item: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32,
}

/// `IMMDevice`.
#[repr(C)]
struct DeviceVtbl {
    unknown: UnknownVtbl,
    activate: unsafe extern "system" fn(
        *mut c_void,
        *const Guid,
        u32,
        *const PropVariant,
        *mut *mut c_void,
    ) -> i32,
    open_property_store: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32,
    get_id: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> i32,
}

/// `IPropertyStore`.
#[repr(C)]
struct PropertyStoreVtbl {
    unknown: UnknownVtbl,
    get_count: usize,
    get_at: usize,
    get_value: unsafe extern "system" fn(*mut c_void, *const PropertyKey, *mut PropVariant) -> i32,
}

/// `IAudioClient`.
#[repr(C)]
struct AudioClientVtbl {
    unknown: UnknownVtbl,
    initialize: unsafe extern "system" fn(
        *mut c_void,
        i32,
        u32,
        i64,
        i64,
        *const WaveFormatEx,
        *const Guid,
    ) -> i32,
    get_buffer_size: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32,
    get_stream_latency: usize,
    get_current_padding: usize,
    is_format_supported: usize,
    get_mix_format: unsafe extern "system" fn(*mut c_void, *mut *mut WaveFormatEx) -> i32,
    get_device_period: usize,
    start: Method0,
    stop: Method0,
    reset: usize,
    set_event_handle: usize,
    get_service: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32,
}

/// `IAudioCaptureClient`.
#[repr(C)]
struct CaptureClientVtbl {
    unknown: UnknownVtbl,
    get_buffer: unsafe extern "system" fn(
        *mut c_void,
        *mut *mut u8,
        *mut u32,
        *mut u32,
        *mut u64,
        *mut u64,
    ) -> i32,
    release_buffer: unsafe extern "system" fn(*mut c_void, u32) -> i32,
    get_next_packet_size: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32,
}

macro_rules! vtables {
    ($($vtable:ty),*) => {
        $(impl Vtable for $vtable {
            fn unknown(&self) -> &UnknownVtbl {
                &self.unknown
            }
        })*
    };
}

vtables!(
    EnumeratorVtbl,
    CollectionVtbl,
    DeviceVtbl,
    PropertyStoreVtbl,
    AudioClientVtbl,
    CaptureClientVtbl
);

// The DLLs are named in full and bound by the linker itself (raw-dylib), so building needs no
// import library, on the GNU and MSVC targets alike. 32-bit x86 imports them by their plain
// names, as the DLLs export them.
#[cfg_attr(
    target_arch = "x86",
    link(
        name = "ole32.dll",
        kind = "raw-dylib",
        modifiers = "+verbatim",
        import_name_type = "undecorated"
    )
)]
#[cfg_attr(
    not(target_arch = "x86"),
    link(name = "ole32.dll", kind = "raw-dylib", modifiers = "+verbatim")
)]
unsafe extern "system" {
    fn CoInitializeEx(reserved: *mut c_void, coinit: u32) -> i32;
    fn CoUninitialize();
    fn CoCreateInstance(
        clsid: *const Guid,
        outer: *mut c_void,
        context: u32,
        iid: *const Guid,
        object: *mut *mut c_void,
    ) -> i32;
    fn CoTaskMemFree(memory: *mut c_void);
    fn PropVariantClear(value: *mut PropVariant) -> i32;
}

#[cfg_attr(
    target_arch = "x86",
    link(
        name = "kernel32.dll",
        kind = "raw-dylib",
        modifiers = "+verbatim",
        import_name_type = "undecorated"
    )
)]
#[cfg_attr(
    not(target_arch = "x86"),
    link(name = "kernel32.dll", kind = "raw-dylib", modifiers = "+verbatim")
)]
unsafe extern "system" {
    fn CreateEventW(
        attributes: *mut c_void,
        manual_reset: i32,
        initial_state: i32,
        name: *const u16,
    ) -> *mut c_void;
    fn SetEvent(event: *mut c_void) -> i32;
    fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
}

/// This thread's membership of COM's multithreaded apartment, which it leaves when dropped.
pub(crate) struct Apartment {
    _this_thread: PhantomData<*mut ()>,
}

impl Apartment {
    pub(crate) fn enter() -> Result<Apartment, Hresult> {
        // SAFETY: CoInitializeEx takes a reserved null pointer and a plain flag. Any thread may
        // call it, and a success, S_OK or S_FALSE, is balanced by CoUninitialize when dropped.
        let code = unsafe { CoInitializeEx(ptr::null_mut(), COINIT_MULTITHREADED) };
        Hresult(code).ok()?;
        Ok(Apartment {
            _this_thread: PhantomData,
        })
    }

    pub(crate) fn enumerator(&self) -> Result<Enumerator<'_>, Hresult> {
        let mut object = ptr::null_mut();
        // SAFETY: the class and interface ids are statics, `object` is a valid place for the
        // result, and this thread is in an apartment while `self` lives.
        let code = unsafe {
            CoCreateInstance(
                &CLSID_MM_DEVICE_ENUMERATOR,
                ptr::null_mut(),
                CLSCTX_ALL,
                &IID_IMM_DEVICE_ENUMERATOR,
                &mut object,
            )
        };
        Hresult(code).ok()?;
        // SAFETY: on success CoCreateInstance left an IMMDeviceEnumerator pointer there, with a
        // reference for the caller.
        unsafe { Object::from_raw(object) }.map(Enumerator)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: balances the successful CoInitializeEx of `enter`, on the same thread since an
        // apartment isn't `Send`. Every interface pointer borrows the apartment, so all were
        // released before this.
        unsafe { CoUninitialize() };
    }
}

/// A reference to a COM object whose vtable is `V`, released when dropped.
struct Object<'a, V: Vtable> {
    this: NonNull<c_void>,
    vtbl: NonNull<V>,
    _apartment: PhantomData<&'a Apartment>,
}

impl<V: Vtable> Object<'_, V> {
    /// Takes over the reference that `this` carries.
    ///
    /// # Safety
    ///
    /// `this` is null, or points to a live COM object of the interface `V` describes, with one
    /// reference the caller hands over, made on this thread while its apartment is entered.
    unsafe fn from_raw(this: *mut c_void) -> Result<Self, Hresult> {
        let this = NonNull::new(this).ok_or(E_POINTER)?;
        // SAFETY: by the contract `this` points to a COM object, which begins with a pointer to
        // its vtable, aligned and readable as every COM object's is.
        let vtbl = unsafe { this.cast::<*mut V>().read() };
        Ok(Object {
            this,
            vtbl: NonNull::new(vtbl).ok_or(E_POINTER)?,
            _apartment: PhantomData,
        })
    }

    fn vtbl(&self) -> &V {
        // SAFETY: a COM object's vtable stays valid and unchanged while the object lives, and
        // this wrapper holds a reference to the object until it's dropped. `V` lists a prefix of
        // the interface's methods, so the real vtable is at least as long.
        unsafe { self.vtbl.as_ref() }
    }

    fn this(&self) -> *mut c_void {
        self.this.as_ptr()
    }
}

impl<V: Vtable> Drop for Object<'_, V> {
    fn drop(&mut self) {
        let release = self.vtbl().unknown().release;
        // SAFETY: this wrapper owns one reference to the object and gives it back once, here.
        unsafe { release(self.this()) };
    }
}

pub(crate) struct Enumerator<'a>(Object<'a, EnumeratorVtbl>);

impl<'a> Enumerator<'a> {
    /// The default capture endpoint for the console role, or none when the system has none.
    pub(crate) fn default_capture(&self) -> Result<Option<Endpoint<'a>>, Hresult> {
        let get = self.0.vtbl().get_default_audio_endpoint;
        let mut device = ptr::null_mut();
        // SAFETY: `this` is a live IMMDeviceEnumerator, the flow and role are valid values, and
        // `device` is a valid place for the result.
        let code = Hresult(unsafe { get(self.0.this(), E_CAPTURE, E_CONSOLE, &mut device) });
        if code == E_NOTFOUND {
            return Ok(None);
        }
        code.ok()?;
        // SAFETY: on success GetDefaultAudioEndpoint left an IMMDevice pointer with a reference
        // for the caller.
        unsafe { Object::from_raw(device) }.map(|device| Some(Endpoint(device)))
    }

    /// The capture endpoints that are present and enabled.
    pub(crate) fn active_captures(&self) -> Result<Captures<'a>, Hresult> {
        let enumerate = self.0.vtbl().enum_audio_endpoints;
        let mut collection = ptr::null_mut();
        // SAFETY: as in `default_capture`, with a valid state mask.
        let code = unsafe {
            enumerate(
                self.0.this(),
                E_CAPTURE,
                DEVICE_STATE_ACTIVE,
                &mut collection,
            )
        };
        Hresult(code).ok()?;
        // SAFETY: on success EnumAudioEndpoints left an IMMDeviceCollection pointer with a
        // reference for the caller.
        unsafe { Object::from_raw(collection) }.map(Captures)
    }
}

/// A list of endpoints, read one at a time.
pub(crate) struct Captures<'a>(Object<'a, CollectionVtbl>);

impl<'a> Captures<'a> {
    /// How many endpoints the list says it has, which the caller bounds.
    pub(crate) fn count(&self) -> Result<u32, Hresult> {
        let get_count = self.0.vtbl().get_count;
        let mut count = 0;
        // SAFETY: `this` is a live IMMDeviceCollection and `count` a valid place for the count.
        Hresult(unsafe { get_count(self.0.this(), &mut count) }).ok()?;
        Ok(count)
    }

    /// The endpoint at `index`, which must be below the count.
    pub(crate) fn get(&self, index: u32) -> Result<Endpoint<'a>, Hresult> {
        let item = self.0.vtbl().item;
        let mut device = ptr::null_mut();
        // SAFETY: `this` is a live IMMDeviceCollection and `device` a valid place for the
        // result. An index past the end is an error the call returns, not undefined behaviour.
        Hresult(unsafe { item(self.0.this(), index, &mut device) }).ok()?;
        // SAFETY: on success Item left an IMMDevice pointer with a reference for the caller.
        unsafe { Object::from_raw(device) }.map(Endpoint)
    }
}

/// An audio endpoint, a device as Windows lists it.
pub(crate) struct Endpoint<'a>(Object<'a, DeviceVtbl>);

impl<'a> Endpoint<'a> {
    /// The endpoint's id, or none when it isn't valid UTF-16.
    pub(crate) fn id(&self) -> Result<Option<String>, Hresult> {
        let get_id = self.0.vtbl().get_id;
        let mut text = ptr::null_mut();
        // SAFETY: `this` is a live IMMDevice and `text` a valid place for the string pointer.
        Hresult(unsafe { get_id(self.0.this(), &mut text) }).ok()?;
        let text = TaskMemory(NonNull::new(text.cast()).ok_or(E_POINTER)?);
        // SAFETY: on success GetId left a null-terminated string that the caller frees with
        // CoTaskMemFree, which `text` does once the string has been read.
        Ok(unsafe { read_wide(text.0.cast()) })
    }

    /// The endpoint's friendly name, else its description.
    pub(crate) fn name(&self) -> Result<Option<String>, Hresult> {
        let open = self.0.vtbl().open_property_store;
        let mut store = ptr::null_mut();
        // SAFETY: `this` is a live IMMDevice, STGM_READ a valid mode and `store` a valid place
        // for the result.
        Hresult(unsafe { open(self.0.this(), STGM_READ, &mut store) }).ok()?;
        // SAFETY: on success OpenPropertyStore left an IPropertyStore pointer with a reference
        // for the caller.
        let store: Object<'a, PropertyStoreVtbl> = unsafe { Object::from_raw(store) }?;
        for key in [&PKEY_DEVICE_FRIENDLY_NAME, &PKEY_DEVICE_DESC] {
            if let Some(name) = text_property(&store, key)? {
                return Ok(Some(name));
            }
        }
        Ok(None)
    }

    /// A new, uninitialized audio client of the endpoint.
    pub(crate) fn activate_client(&self) -> Result<AudioClient<'a>, Hresult> {
        let activate = self.0.vtbl().activate;
        let mut client = ptr::null_mut();
        // SAFETY: `this` is a live IMMDevice, the interface id a static, the activation
        // parameters null as allowed for IAudioClient, and `client` a valid place for the
        // result.
        let code = unsafe {
            activate(
                self.0.this(),
                &IID_IAUDIO_CLIENT,
                CLSCTX_ALL,
                ptr::null(),
                &mut client,
            )
        };
        Hresult(code).ok()?;
        // SAFETY: on success Activate left an IAudioClient pointer with a reference for the
        // caller.
        unsafe { Object::from_raw(client) }.map(AudioClient)
    }
}

/// Reads a string property, or none when the store has no string under `key`.
fn text_property(
    store: &Object<'_, PropertyStoreVtbl>,
    key: &PropertyKey,
) -> Result<Option<String>, Hresult> {
    let get_value = store.vtbl().get_value;
    let mut value = PropVariant::empty();
    // SAFETY: `this` is a live IPropertyStore, `key` a valid key, and `value` an empty
    // PROPVARIANT for the store to fill. `value` clears whatever it holds when dropped.
    Hresult(unsafe { get_value(store.this(), key, &mut value) }).ok()?;
    Ok(value.text())
}

impl PropVariant {
    fn empty() -> PropVariant {
        PropVariant {
            vt: 0,
            reserved: [0; 3],
            value: PropValue { blob: [0; 2] },
        }
    }

    /// The string the value holds, when its type is a wide string.
    fn text(&self) -> Option<String> {
        if self.vt != VT_LPWSTR {
            return None;
        }
        // SAFETY: a PROPVARIANT of type VT_LPWSTR holds its string's pointer in this field.
        let text = NonNull::new(unsafe { self.value.text })?;
        // SAFETY: the pointer is a null-terminated string the PROPVARIANT owns until it's
        // cleared, which happens only when `self` is dropped.
        unsafe { read_wide(text) }
    }
}

impl Drop for PropVariant {
    fn drop(&mut self) {
        // SAFETY: `self` is a PROPVARIANT that is empty or was filled by the system, and
        // PropVariantClear frees what it holds and leaves it empty.
        unsafe { PropVariantClear(self) };
    }
}

/// Memory the system allocated for the caller, freed with CoTaskMemFree when dropped.
struct TaskMemory(NonNull<c_void>);

impl Drop for TaskMemory {
    fn drop(&mut self) {
        // SAFETY: the memory came from a call whose contract hands it to the caller to free with
        // CoTaskMemFree, and this wrapper frees it once.
        unsafe { CoTaskMemFree(self.0.as_ptr()) };
    }
}

/// Reads a null-terminated UTF-16 string of at most [`MAX_UNITS`] units. Returns none for a
/// longer string or one that isn't valid UTF-16.
///
/// # Safety
///
/// `text` points to a null-terminated UTF-16 string that stays valid during the call.
unsafe fn read_wide(text: NonNull<u16>) -> Option<String> {
    let mut units = Vec::new();
    for index in 0..MAX_UNITS {
        let unit = text.as_ptr().wrapping_add(index);
        // SAFETY: by the contract the string is readable up to its terminator, and this loop
        // stops there, so `unit` lies within it.
        let unit = unsafe { unit.read_unaligned() };
        if unit == 0 {
            return String::from_utf16(&units).ok();
        }
        units.push(unit);
    }
    None
}

pub(crate) struct AudioClient<'a>(Object<'a, AudioClientVtbl>);

impl<'a> AudioClient<'a> {
    /// Opens the stream that `capture` describes.
    pub(crate) fn initialize(&self, capture: &SharedCapture) -> Result<(), Hresult> {
        let initialize = self.0.vtbl().initialize;
        // SAFETY: `this` is a live IAudioClient, the format a valid WAVEFORMATEX that lives in
        // `capture` during the call, the mode, flags, buffer and periodicity a combination that
        // Initialize documents for shared mode without events, and the session null for the
        // default one.
        let code = unsafe {
            initialize(
                self.0.this(),
                AUDCLNT_SHAREMODE_SHARED,
                capture.flags,
                capture.buffer,
                capture.periodicity,
                &capture.format,
                ptr::null(),
            )
        };
        Hresult(code).ok()
    }

    /// The rate of the engine's own format for the endpoint.
    pub(crate) fn mix_rate(&self) -> Result<u32, Hresult> {
        let get_mix_format = self.0.vtbl().get_mix_format;
        let mut format = ptr::null_mut();
        // SAFETY: `this` is a live IAudioClient and `format` a valid place for the pointer.
        Hresult(unsafe { get_mix_format(self.0.this(), &mut format) }).ok()?;
        let format = NonNull::new(format).ok_or(E_POINTER)?;
        let memory = TaskMemory(format.cast());
        // SAFETY: on success GetMixFormat left a WAVEFORMATEX or a larger structure that begins
        // with one, which the caller frees with CoTaskMemFree, as `memory` does after this read.
        // The struct is packed, so any address is aligned for it.
        let rate = unsafe { format.read() }.samples_per_sec;
        drop(memory);
        Ok(rate)
    }

    /// The engine buffer's size in frames, which the caller bounds.
    pub(crate) fn buffer_frames(&self) -> Result<u32, Hresult> {
        let get_buffer_size = self.0.vtbl().get_buffer_size;
        let mut frames = 0;
        // SAFETY: `this` is a live, initialized IAudioClient and `frames` a valid place.
        Hresult(unsafe { get_buffer_size(self.0.this(), &mut frames) }).ok()?;
        Ok(frames)
    }

    pub(crate) fn capture_client(&self) -> Result<CaptureClient<'a>, Hresult> {
        let get_service = self.0.vtbl().get_service;
        let mut capture = ptr::null_mut();
        // SAFETY: `this` is a live, initialized IAudioClient, the interface id a static, and
        // `capture` a valid place for the result.
        let code = unsafe { get_service(self.0.this(), &IID_IAUDIO_CAPTURE_CLIENT, &mut capture) };
        Hresult(code).ok()?;
        // SAFETY: on success GetService left an IAudioCaptureClient pointer with a reference for
        // the caller.
        unsafe { Object::from_raw(capture) }.map(CaptureClient)
    }

    pub(crate) fn start(&self) -> Result<(), Hresult> {
        let start = self.0.vtbl().start;
        // SAFETY: `this` is a live, initialized IAudioClient.
        Hresult(unsafe { start(self.0.this()) }).ok()
    }

    pub(crate) fn stop(&self) -> Result<(), Hresult> {
        let stop = self.0.vtbl().stop;
        // SAFETY: `this` is a live IAudioClient.
        Hresult(unsafe { stop(self.0.this()) }).ok()
    }
}

pub(crate) struct CaptureClient<'a>(Object<'a, CaptureClientVtbl>);

/// A packet of captured audio, valid until it's released.
pub(crate) struct Packet<'p> {
    pub flags: u32,
    pub frames: u32,
    /// The packet's bytes, or none when the engine described more frames than its buffer
    /// holds or than `limit` bytes, or gave no data.
    pub bytes: Option<&'p [u8]>,
}

impl CaptureClient<'_> {
    /// The frames in the next packet, 0 when none is ready.
    pub(crate) fn next_packet_frames(&self) -> Result<u32, Hresult> {
        let get_next_packet_size = self.0.vtbl().get_next_packet_size;
        let mut frames = 0;
        // SAFETY: `this` is a live IAudioCaptureClient and `frames` a valid place.
        Hresult(unsafe { get_next_packet_size(self.0.this(), &mut frames) }).ok()?;
        Ok(frames)
    }

    /// Hands the next packet to `read` and releases it after. Returns none when no packet was
    /// ready. Frames are `frame_bytes` long, the engine's buffer holds `buffer_frames`, and a
    /// packet may be at most `limit` bytes.
    pub(crate) fn read_packet<R>(
        &self,
        frame_bytes: usize,
        buffer_frames: u32,
        limit: usize,
        read: impl FnOnce(Packet<'_>) -> R,
    ) -> Result<Option<R>, Hresult> {
        let get_buffer = self.0.vtbl().get_buffer;
        let mut data = ptr::null_mut();
        let mut frames = 0;
        let mut flags = 0;
        // SAFETY: `this` is a live IAudioCaptureClient, the first three places are valid, and
        // the two positions are optional and null.
        let code = Hresult(unsafe {
            get_buffer(
                self.0.this(),
                &mut data,
                &mut frames,
                &mut flags,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        });
        if code == AUDCLNT_S_BUFFER_EMPTY {
            return Ok(None);
        }
        code.ok()?;
        let length = byte_length(frames, frame_bytes, limit).filter(|_| frames <= buffer_frames);
        let bytes = match length {
            Some(0) => Some(&[][..]),
            Some(length) if !data.is_null() => {
                // SAFETY: on success GetBuffer left a pointer to `frames` frames of the
                // client's format, `length` bytes, which `byte_length` keeps within
                // `isize::MAX`. The engine keeps them unchanged until ReleaseBuffer, which runs
                // only after `read` returns, and `read` can't keep the slice past that.
                Some(unsafe { std::slice::from_raw_parts(data.cast_const(), length) })
            }
            _ => None,
        };
        let valid = bytes.is_some();
        let result = read(Packet {
            flags,
            frames,
            bytes,
        });
        let release_buffer = self.0.vtbl().release_buffer;
        let released = if valid { frames } else { 0 };
        // SAFETY: `this` is a live IAudioCaptureClient, and `released` is the packet's frames,
        // or 0 for a packet that couldn't be read.
        Hresult(unsafe { release_buffer(self.0.this(), released) }).ok()?;
        Ok(Some(result))
    }
}

/// An auto-reset event, closed when dropped. A duplicate refers to the same event, and any
/// thread may set or wait on it. Only Capta's own threads use it, the audio engine never sees
/// it.
pub(crate) struct Event(OwnedHandle);

impl Event {
    pub(crate) fn new() -> io::Result<Event> {
        // SAFETY: null attributes give the default security, the two flags are plain values, and
        // a null name makes the event unnamed.
        let handle = unsafe { CreateEventW(ptr::null_mut(), 0, 0, ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateEventW returned a new, open handle that nothing else owns, and an event
        // needs only CloseHandle to clean up.
        Ok(Event(unsafe { OwnedHandle::from_raw_handle(handle) }))
    }

    pub(crate) fn try_clone(&self) -> io::Result<Event> {
        self.0.try_clone().map(Event)
    }

    pub(crate) fn set(&self) -> io::Result<()> {
        // SAFETY: the handle is an open event handle that `self` owns.
        if unsafe { SetEvent(self.raw()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Waits for the event for at most `timeout`. Returns whether it was set.
    pub(crate) fn wait(&self, timeout: Duration) -> io::Result<bool> {
        let milliseconds = u32::try_from(timeout.as_millis())
            .unwrap_or(INFINITE)
            .min(INFINITE - 1);
        // SAFETY: the handle is an open event handle that `self` owns.
        match unsafe { WaitForSingleObject(self.raw(), milliseconds) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(io::Error::last_os_error()),
        }
    }

    fn raw(&self) -> *mut c_void {
        self.0.as_raw_handle()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_match_the_headers() {
        assert_eq!(size_of::<Guid>(), 16);
        assert_eq!(size_of::<PropertyKey>(), 20);
        assert_eq!(size_of::<WaveFormatEx>(), 18);
        let (size, align) = if cfg!(target_pointer_width = "64") {
            (24, 8)
        } else {
            (16, 8)
        };
        assert_eq!(
            (size_of::<PropVariant>(), align_of::<PropVariant>()),
            (size, align)
        );
        let id = Guid::from_u128(0x1cb9ad4c_dbfa_4c32_b178_c2f568a703b2);
        assert_eq!(
            (id.data1, id.data2, id.data3, id.data4),
            (
                0x1cb9ad4c,
                0xdbfa,
                0x4c32,
                [0xb1, 0x78, 0xc2, 0xf5, 0x68, 0xa7, 0x03, 0xb2]
            )
        );

        use std::mem::offset_of as at;
        assert_eq!(
            [
                at!(Guid, data1),
                at!(Guid, data2),
                at!(Guid, data3),
                at!(Guid, data4)
            ],
            [0, 4, 6, 8]
        );
        assert_eq!([at!(PropertyKey, fmtid), at!(PropertyKey, pid)], [0, 16]);
        assert_eq!([at!(PropVariant, vt), at!(PropVariant, value)], [0, 8]);
        assert_eq!(
            [
                at!(WaveFormatEx, format_tag),
                at!(WaveFormatEx, channels),
                at!(WaveFormatEx, samples_per_sec),
                at!(WaveFormatEx, avg_bytes_per_sec),
                at!(WaveFormatEx, block_align),
                at!(WaveFormatEx, bits_per_sample),
                at!(WaveFormatEx, size),
            ],
            [0, 2, 4, 8, 12, 14, 16]
        );

        // Each method's place in its vtable, in function pointers, as the headers list them.
        let slot = |offset: usize| offset / size_of::<usize>();
        assert_eq!(
            [
                slot(at!(UnknownVtbl, query_interface)),
                slot(at!(UnknownVtbl, add_ref)),
                slot(at!(UnknownVtbl, release)),
            ],
            [0, 1, 2]
        );
        assert_eq!(
            [
                slot(at!(EnumeratorVtbl, enum_audio_endpoints)),
                slot(at!(EnumeratorVtbl, get_default_audio_endpoint)),
            ],
            [3, 4]
        );
        assert_eq!(
            [
                slot(at!(CollectionVtbl, get_count)),
                slot(at!(CollectionVtbl, item))
            ],
            [3, 4]
        );
        assert_eq!(
            [
                slot(at!(DeviceVtbl, activate)),
                slot(at!(DeviceVtbl, open_property_store)),
                slot(at!(DeviceVtbl, get_id)),
            ],
            [3, 4, 5]
        );
        assert_eq!(slot(at!(PropertyStoreVtbl, get_value)), 5);
        assert_eq!(
            [
                slot(at!(AudioClientVtbl, initialize)),
                slot(at!(AudioClientVtbl, get_buffer_size)),
                slot(at!(AudioClientVtbl, get_mix_format)),
                slot(at!(AudioClientVtbl, start)),
                slot(at!(AudioClientVtbl, stop)),
                slot(at!(AudioClientVtbl, get_service)),
            ],
            [3, 4, 8, 10, 11, 14]
        );
        assert_eq!(
            [
                slot(at!(CaptureClientVtbl, get_buffer)),
                slot(at!(CaptureClientVtbl, release_buffer)),
                slot(at!(CaptureClientVtbl, get_next_packet_size)),
            ],
            [3, 4, 5]
        );
    }

    #[test]
    fn an_event_wakes_a_waiting_thread() {
        let event = Event::new().unwrap();
        assert!(!event.wait(Duration::from_millis(1)).unwrap());
        let other = event.try_clone().unwrap();
        let setter = std::thread::spawn(move || other.set().unwrap());
        assert!(event.wait(Duration::from_secs(5)).unwrap());
        setter.join().unwrap();
        // Auto-reset, so the wake was used up.
        assert!(!event.wait(Duration::from_millis(1)).unwrap());
    }

    #[test]
    fn the_apartment_lists_capture_endpoints() {
        let apartment = Apartment::enter().unwrap();
        let enumerator = apartment.enumerator().unwrap();
        let captures = enumerator.active_captures().unwrap();
        for index in 0..captures.count().unwrap() {
            let endpoint = captures.get(index).unwrap();
            assert!(endpoint.id().unwrap().is_some());
            let _ = endpoint.name().unwrap();
        }
    }

    #[test]
    fn every_stream_is_shared_and_polled_with_a_two_second_buffer() {
        for rate in [8_000, 16_000, 44_100, 48_000, 384_000] {
            let capture = SharedCapture::mono_f32(rate);
            assert_eq!(
                capture.flags & EVENT_CALLBACK,
                0,
                "no event-driven buffering"
            );
            assert_eq!(capture.periodicity, 0, "shared mode takes no periodicity");
            assert_eq!(capture.buffer, 20_000_000);
            let format = capture.format;
            let (tag, channels, samples, bytes, align, bits, size) = (
                format.format_tag,
                format.channels,
                format.samples_per_sec,
                format.avg_bytes_per_sec,
                format.block_align,
                format.bits_per_sample,
                format.size,
            );
            assert_eq!(
                (tag, channels, samples, bytes, align, bits, size),
                (WAVE_FORMAT_IEEE_FLOAT, 1, rate, rate * 4, 4, 32, 0)
            );
        }
    }
}
