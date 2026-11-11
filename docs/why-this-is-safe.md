# Why this is safe

Capta's `unsafe` code calls the operating system on Windows and macOS, and does nothing else.
It lives in two files, `src/wasapi/com.rs` and `src/coreaudio/ffi.rs`, behind safe wrappers
that the rest of the crate uses. On Linux the crate has no `unsafe` code and forbids it.

The crate denies `unsafe_op_in_unsafe_fn`, and Clippy's `undocumented_unsafe_blocks` and
`multiple_unsafe_ops_per_block` are on, so every block holds one unsafe operation with a
`// SAFETY:` comment above it. This page lists every block, by file and function, with the
contract it relies on. Where several blocks rest on the same argument, it's made once at the
top of the file's section.

## What every call relies on

- Each function, interface and structure is declared by hand from the system's SDK headers,
  with the headers' calling convention (`extern "system"` on Windows, which is `stdcall` on
  32-bit x86, and `extern "C"` on macOS). The tests `layouts_match_the_headers` check the size
  of every structure and the offset of every field against the headers, and the place of every
  method in every vtable, on the system it's for.
- A call's out-values are used only after its status says it succeeded. A failure becomes an
  `Error`, and what the call might have left in an out-pointer is ignored.
- Everything the system hands back is checked before use. Counts and sizes have limits of
  their own, independent of anything else the system reports, and are compared with the room
  they describe. Strings are read up to a limit. A reply that doesn't fit is an error, never a
  panic or an allocation of what the system claims.
- Of a structure the system fills, Capta reads only the fields the system's documentation
  says it sets, field by field through raw pointers, never the whole structure.
- No callback Capta gives the system can unwind. The one callback, macOS's buffer callback, is
  `extern "C"`, so a panic in it would abort rather than unwind into the system, and it has no
  code that panics.

## Windows, `src/wasapi/com.rs`

### Threads and apartments

Every interface pointer is made and used on one thread, the startup thread of `devices()` or a
capture's thread, after it enters COM's multithreaded apartment with `Apartment::enter`. The
wrappers hold `NonNull` pointers and a `PhantomData<&Apartment>`, so they're neither `Send` nor
`Sync`, and the compiler keeps every one from outliving the `Apartment`, whose drop calls
`CoUninitialize`. The interfaces Capta uses are free-threaded objects of the audio service, so
calling them from the thread that made them is always allowed.

The one value that crosses threads is `Event`, a kernel event handle in an `OwnedHandle`, which
any thread may set or wait on. Only Capta's own threads use it, to end the capture thread's
pause when the `Capture` handle drops. The audio engine never sees it, so its lifetime matters
only to Capta, and each `OwnedHandle` closes its own handle once.

### Interface pointers and their references

An interface pointer is a pointer to an object whose first field points to its vtable. A COM
method that returns an interface through an out-pointer leaves it with one reference for the
caller (AddRef'd), and null on failure. `Object::from_raw` takes that reference over, and
`Object`'s drop gives it back once with `Release`. Each vtable struct lists the interface's
methods in the header's order, up to the last one Capta calls, so it's a prefix of the real
vtable. Methods Capta doesn't call are `usize` slots, the size of a function pointer.

A method call reads its function pointer from the vtable in safe code and calls it in one
unsafe block. That call is sound when `this` is a live object of the interface, which holds
while the wrapper does, and every pointer argument is valid for the call, which holds because
each points to a local, a `static` or memory the wrapper owns.

### The blocks

| Function | Call | Why it's sound |
|---|---|---|
| `Apartment::enter` | `CoInitializeEx(NULL, COINIT_MULTITHREADED)` | Takes a reserved null pointer and a flag, from any thread. A success, `S_OK` or `S_FALSE`, is balanced by `CoUninitialize` in the drop. A failure, such as `RPC_E_CHANGED_MODE`, makes no `Apartment`, so nothing balances it. |
| `Apartment::enumerator` | `CoCreateInstance(CLSID_MMDeviceEnumerator, …, IID_IMMDeviceEnumerator)` | The class and interface ids are `static`s, there's no outer object, and the out-pointer is a local. The thread is in the apartment while `self` lives. |
| `Apartment::enumerator` | `Object::from_raw` | On success the result is an `IMMDeviceEnumerator` pointer with a reference for the caller, which `from_raw` takes over. |
| `Drop for Apartment` | `CoUninitialize()` | Balances the successful `CoInitializeEx`, on the same thread since `Apartment` isn't `Send`. Every interface wrapper borrows the apartment, so all were released first. |
| `Object::from_raw` (an `unsafe fn`) | Reads the vtable pointer, `this.cast::<*mut V>().read()` | The caller promises a live COM object of interface `V`. Its first field is the vtable pointer, aligned and readable. A null `this` returns an error before the read. |
| `Object::vtbl` | `self.vtbl.as_ref()` | A COM object's vtable stays valid and unchanged while the object lives, and the wrapper holds a reference until dropped. The returned reference borrows the wrapper. |
| `Drop for Object` | `Release(this)` | The wrapper owns one reference, given back once. |
| `Enumerator::default_capture` | `IMMDeviceEnumerator::GetDefaultAudioEndpoint(eCapture, eConsole, &device)` | Valid flow and role values and a local out-pointer. `E_NOTFOUND`, no capture device, is an answer, not an error. |
| `Enumerator::default_capture` | `Object::from_raw` | On success, an `IMMDevice` with a reference for the caller. |
| `Enumerator::active_captures` | `IMMDeviceEnumerator::EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE, &collection)` | As above, with a valid state mask. |
| `Enumerator::active_captures` | `Object::from_raw` | On success, an `IMMDeviceCollection` with a reference for the caller. |
| `Captures::count` | `IMMDeviceCollection::GetCount(&count)` | A local out-value. The caller refuses a count above 4096 before it reads any item, so the count never sizes an allocation or a loop. |
| `Captures::get` | `IMMDeviceCollection::Item(index, &device)` | The out-pointer is a local. The caller passes indices below the bounded count, and an index past the end would be an error the call returns, not undefined behaviour. A failed item fails the listing rather than being read. |
| `Captures::get` | `Object::from_raw` | On success, an `IMMDevice` with a reference for the caller. |
| `Endpoint::id` | `IMMDevice::GetId(&text)` | A local out-pointer. On success it holds a null-terminated string allocated with `CoTaskMemAlloc`, which `TaskMemory` frees after the read. |
| `Endpoint::id` | `read_wide(text)` | The string from `GetId` is null-terminated and stays allocated until `TaskMemory` drops, after the read. |
| `Endpoint::name` | `IMMDevice::OpenPropertyStore(STGM_READ, &store)` | A valid access mode and a local out-pointer. |
| `Endpoint::name` | `Object::from_raw` | On success, an `IPropertyStore` with a reference for the caller. |
| `Endpoint::activate_client` | `IMMDevice::Activate(IID_IAudioClient, CLSCTX_ALL, NULL, &client)` | The interface id is a `static`, and `IAudioClient` takes no activation parameters, so null is allowed. |
| `Endpoint::activate_client` | `Object::from_raw` | On success, an `IAudioClient` with a reference for the caller. |
| `text_property` | `IPropertyStore::GetValue(key, &value)` | The key is a `static` and `value` an empty `PROPVARIANT` for the store to fill. Whatever it holds is cleared by `PropVariant`'s drop. |
| `PropVariant::text` | Reads the union field `value.text` | Done only when `vt` is `VT_LPWSTR`, the type whose payload is that pointer. |
| `PropVariant::text` | `read_wide(text)` | A `VT_LPWSTR` value owns its null-terminated string until it's cleared, which happens when `self` drops, after the read. |
| `Drop for PropVariant` | `PropVariantClear(self)` | `self` is a valid `PROPVARIANT`, empty or filled by the system. Clearing frees what it holds and leaves it empty, and an empty one needs nothing. |
| `Drop for TaskMemory` | `CoTaskMemFree(memory)` | The memory came from `GetId` or `GetMixFormat`, whose contracts hand it to the caller to free with `CoTaskMemFree`, and the wrapper frees it once. |
| `read_wide` (an `unsafe fn`) | `unit.read_unaligned()` | The caller promises a null-terminated UTF-16 string. The loop stops at the terminator or after 32 Ki units, so each read lies within the string. The pointer is formed with `wrapping_add`, so forming it is never undefined, and the read doesn't need alignment. |
| `AudioClient::initialize` | `IAudioClient::Initialize(SHARED, AUTOCONVERTPCM \| SRC_DEFAULT_QUALITY, 2 s, 0, &format, NULL)` | Every call takes its arguments from `SharedCapture`, so every attempt opens the same mode. `format` is a packed `WAVEFORMATEX` for one channel of floats, valid during the call. The stream is shared and polled, without `EVENTCALLBACK`, the mode in which Initialize documents a buffer duration of the client's choosing and requires a periodicity of 0, and Microsoft's capture example uses. A null session joins the default one. A refused format is an error the caller handles by making a new client. |
| `AudioClient::mix_rate` | `IAudioClient::GetMixFormat(&format)` | A local out-pointer. On success it points to a `WAVEFORMATEX`, or a larger structure that begins with one, allocated with `CoTaskMemAlloc`. |
| `AudioClient::mix_rate` | `format.read()` | The memory holds at least a `WAVEFORMATEX`, and the struct is packed, so any address is aligned for it. `TaskMemory` frees it after the read. |
| `AudioClient::buffer_frames` | `IAudioClient::GetBufferSize(&frames)` | A local out-value, on an initialized client. The caller refuses 0 and more than 1,536,000 frames, 4 s at the highest rate. |
| `AudioClient::capture_client` | `IAudioClient::GetService(IID_IAudioCaptureClient, &capture)` | A `static` interface id and a local out-pointer, on an initialized client. |
| `AudioClient::capture_client` | `Object::from_raw` | On success, an `IAudioCaptureClient` with a reference for the caller. |
| `AudioClient::start` | `IAudioClient::Start()` | An initialized client. |
| `AudioClient::stop` | `IAudioClient::Stop()` | A live client. Stopping a stopped client is allowed and returns `S_FALSE`. |
| `CaptureClient::next_packet_frames` | `IAudioCaptureClient::GetNextPacketSize(&frames)` | A local out-value. |
| `CaptureClient::read_packet` | `IAudioCaptureClient::GetBuffer(&data, &frames, &flags, NULL, NULL)` | Three local out-values, and the two positions are optional. `AUDCLNT_S_BUFFER_EMPTY` means no packet, and nothing is read. |
| `CaptureClient::read_packet` | `slice::from_raw_parts(data, length)` | Made only when `data` isn't null, `frames` is at most the engine buffer's bounded size, and `byte_length` finds `frames × 4` without overflow, within 6,144,000 bytes and within `isize::MAX`, as a slice requires on 32-bit systems too. The engine keeps those bytes, frames of the client's mono float format, unchanged until `ReleaseBuffer`. The slice goes to a closure that can't keep it, since its lifetime is the closure's own, and `ReleaseBuffer` runs only after the closure returns. |
| `CaptureClient::read_packet` | `IAudioCaptureClient::ReleaseBuffer(released)` | Releases the packet's frames, or 0 for a packet that couldn't be read, which the API allows. |
| `Event::new` | `CreateEventW(NULL, FALSE, FALSE, NULL)` | Default security, auto-reset, not set, unnamed. A null result is an error, read from `GetLastError`. |
| `Event::new` | `OwnedHandle::from_raw_handle(handle)` | The handle is new, open and owned by nothing else, and an event needs only `CloseHandle`, which `OwnedHandle` does. |
| `Event::set` | `SetEvent(handle)` | The handle is open while `self` lives. Setting an event from any thread is allowed. |
| `Event::wait` | `WaitForSingleObject(handle, milliseconds)` | The handle is open while `self` lives, and the timeout is finite, below `INFINITE`. |

The two `unsafe extern "system"` blocks declare ole32.dll's and kernel32.dll's functions. They
link by name through raw-dylib, so no import library is involved.

## macOS, `src/coreaudio/ffi.rs`

### Threads

All of the HAL property calls and every Audio Queue call except the buffer callback run on
the startup thread of `devices()` or on a capture's thread. The HAL's property functions may
be called from any thread. A `Queue` is made, started and disposed of on the capture thread,
and it isn't `Send`, so it can't move to another.

The queue is made with a null run loop, so Audio Toolbox calls `on_buffer` on a thread of its
own. That's the only code of Capta's that runs on one of Core Audio's threads. It takes no
lock, allocates nothing, and calls nothing but `AudioQueueEnqueueBuffer`, which the Audio Queue
Services guide calls from the callback to give the buffer back. It shares the `Inbox` with the
capture thread, and everything in the inbox changes through atomics. The capture thread
doesn't wait for the callback, it looks at the ring every 5 ms, so the callback never has to
wake anything.

The ring in the inbox is safe code with one consumer, the capture thread, and it must have
one producer at a time. Apple's documentation says the queue calls back on "one of the audio
queue's internal threads" without saying that two callbacks never overlap, so Capta enforces
it. A callback takes the inbox's `taking` flag with an atomic swap before it pushes and gives
it back after, with acquire and release ordering, so callbacks on different threads see each
other's pushes. A callback that finds the flag taken pushes nothing and ends the capture with
`Protocol`, so overlapping callbacks can't interleave samples unnoticed.

### The inbox's lifetime

`Queue` holds the inbox in an `Arc` and passes `Arc::as_ptr` to `AudioQueueNewInput` as the
callback's user data. The `Arc` is a field of `Queue`, and fields drop after `Drop::drop` has
run, so the inbox outlives the call to `AudioQueueDispose(queue, true)` in that drop.
AudioQueue.h says that once `AudioQueueDispose` returns, the queue no longer invokes any
callbacks, except when it's called from a callback, and it's called on the capture thread,
never from one. So no callback can use the inbox after it's freed. An `Arc` rather than a
`Box` keeps the pointer valid while the `Queue` moves, since a moved `Box` would claim unique
access to memory the callback reads.

### The blocks

| Function | Call | Why it's sound |
|---|---|---|
| `property` | `AudioObjectGetPropertyData(object, &address, 0, NULL, &size, &value)` | `address` is a local with a valid selector, scope and element, there's no qualifier, and `value` is a `T` with `size` bytes of room. The call writes at most `size` bytes and sets `size` to what it wrote. `T` is `u32`, `f64` or a pointer, for which any bit pattern is valid, and a size other than `size_of::<T>()` is an error. A bad object id is an error status, not undefined behaviour. |
| `property_size` | `AudioObjectGetPropertyDataSize(object, &address, 0, NULL, &size)` | A local address and out-value, no qualifier. |
| `devices` | `AudioObjectGetPropertyData(system, devices, …, &size, devices.as_mut_ptr())` | The vector holds `count` ids, `size` bytes, and the call writes at most `size`. The size from `AudioObjectGetPropertyDataSize` must be a whole number of ids and at most 4096 of them before anything is allocated, and the size written must be within what was given. |
| `copied_string` | `CfString::from_owned(string)` | AudioHardware.h says the caller releases the CFString that the UID and name properties return, so the reference is the caller's to take over. A null result is an error. |
| `CfString::new` | `CFStringCreateWithBytes(NULL, text, length, UTF8, false)` | `text` is `length` bytes of UTF-8 that the call copies, and the null allocator is the default one. A null result is an error. |
| `CfString::new` | `CfString::from_owned(string)` | `CFStringCreateWithBytes` follows Core Foundation's create rule, so the caller owns the reference. |
| `CfString::text` | `CFStringGetLength(string)` | `self` holds a reference to a live CFString. |
| `CfString::text` | `CFStringGetMaximumSizeForEncoding(length, UTF8)` | A computation on a length that touches no memory. It returns `kCFNotFound` on overflow, which the conversion to `usize` refuses. |
| `CfString::text` | `CFStringGetCString(string, bytes, size, UTF8)` | `bytes` has `size` bytes of room, at most 64 KiB, and the call writes at most that, the string and a terminating NUL, or fails. The text is read up to the first NUL, and must be UTF-8. |
| `Drop for CfString` | `CFRelease(string)` | `self` owns one reference, given back once. |
| `on_buffer` | `&*user.cast::<Inbox>()` | `user` is `Arc::as_ptr` of the inbox, alive until after the queue is disposed of, as argued above. The reference is shared, the inbox is `Sync`, and nothing makes a `&mut` to it. |
| `on_buffer` | `read_filled(buffer)` | The queue hands the callback one of its own buffers, made by `AudioQueueAllocateBuffer`, which sets its capacity and data pointer, and the queue sets the byte size when it fills it. Nothing writes those until the buffer is enqueued again at the end of the callback. A null buffer returns early. |
| `read_filled` (an `unsafe fn`) | Reads `mAudioDataBytesCapacity` through a raw pointer | The caller promises the field is initialized. Its pointer is the buffer's address plus the field's `offset_of!` in the `repr(C)` struct, so it's inside the buffer and aligned for a `u32`. No reference to the struct is made, and the packet-description fields and `mUserData`, which Apple documents only for buffers with packet descriptions or as the caller's own, are never read. Miri runs `a_filled_buffer_is_read_without_its_unset_fields` on macOS, where only these three fields are written. |
| `read_filled` | Reads `mAudioData` | As above, for the pointer field, aligned for a pointer. |
| `read_filled` | Reads `mAudioDataByteSize` | As above, for the byte size. |
| `on_buffer` | `read_start(start)` | When not null, `start` points to the buffer's time stamp, valid during the callback. AudioTimeStamp's `mFlags` says which of its fields are set. |
| `read_start` (an `unsafe fn`) | Reads `mFlags` through a raw pointer | The caller promises the flags are initialized, and the pointer is the field's offset in the `repr(C)` struct, aligned for a `u32`. |
| `read_start` | Reads `mSampleTime` | Read only when `kAudioTimeStampSampleTimeValid` is set, so the field is set, at an offset aligned for an `f64`. Miri runs `a_time_stamp_is_read_only_as_far_as_its_flags_say`. |
| `on_buffer` | `slice::from_raw_parts(data, byte_size)` | Made only when `data` isn't null and `byte_size` is at most `capacity`. The buffer's data holds `capacity` bytes, the queue filled the first `byte_size`, and they don't change until the buffer is enqueued again, after the slice's last use. A byte size that isn't whole samples is an error. |
| `on_buffer` | `AudioQueueEnqueueBuffer(queue, buffer, 0, NULL)` | `queue` and `buffer` are the ones the queue passed, and an input queue takes no packet descriptions. A failure is recorded and ends the capture. |
| `Queue::new_input` | `AudioQueueNewInput(&format, on_buffer, user, NULL, NULL, 0, &queue)` | `format` is a local linear PCM description of one channel of native-endian floats. `on_buffer` has the callback's signature, `user` is the inbox that outlives the queue, a null run loop has the queue call back on its own thread, the flags are 0 as reserved, and the out-pointer is a local. A null queue on success is an error. |
| `Queue::set_device` | `AudioQueueSetProperty(queue, kAudioQueueProperty_CurrentDevice, &uid, size)` | The queue is live, and the property's value is a `CFStringRef`, passed as a pointer to a local holding it, with its size. The queue retains the string, and the caller's `CfString` outlives the call anyway. |
| `Queue::add_buffers` | `AudioQueueAllocateBuffer(queue, bytes, &buffer)` | A live queue and a local out-pointer. The queue owns the buffer and frees it when disposed of. A null buffer is an error. |
| `Queue::add_buffers` | `AudioQueueEnqueueBuffer(queue, buffer, 0, NULL)` | The buffer is the queue's own and not yet enqueued. |
| `Queue::start` | `AudioQueueStart(queue, NULL)` | A live queue, and a null start time means as soon as possible. |
| `Drop for Queue` | `AudioQueueDispose(queue, true)` | The queue is live and disposed of once, on the capture thread, which isn't the queue's callback thread. Disposing immediately means no callback runs after it returns, so the inbox can be freed after it. `closing` is set first, so a callback that starts during the disposal returns at once. |

The three `unsafe extern "C"` blocks declare the CoreFoundation, CoreAudio and AudioToolbox
functions, linked as the system frameworks they are.

The tests have nine more blocks, in `a_filled_buffer_is_read_without_its_unset_fields` and
`a_time_stamp_is_read_only_as_far_as_its_flags_say`. Each writes one field of a
`MaybeUninit` structure through a pointer formed the way `read_filled` and `read_start` form
theirs, or calls one of the two on a structure whose promised fields it wrote.

## What the code relies on that a test can't show

The SDKs' calling conventions, layouts and constants are copied from the headers, and the
layout tests and the recordings in CI exercise them on the real systems. One promise comes
from Apple's documentation alone, that a disposed queue calls back no more once
`AudioQueueDispose` returns, when it's called from outside a callback, as AudioQueue.h says.
