use crate::error::NegativeErrorExt;
use core::convert::TryInto;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use riot_sys::libc::c_void;
use riot_sys::{coap_optpos_t, coap_pkt_t, coap_resource_t, gcoap_listener_t};

/// Give the caller a way of registering Gcoap handlers into the global Gcoap registry inside a
/// callback. When the callback terminates, the registered handlers are deregistered again,
/// theoretically allowing the registration of non-'static handlers.
///
/// As there is currently no way to unregister handlers, this function panics when the callback
/// terminates. (Otherwise, it'd return the callback's return value).
pub fn scope<'env, F, R>(callback: F) -> R
where
    F: for<'id> FnOnce(&mut RegistrationScope<'env, 'id>) -> R,
{
    let mut r = RegistrationScope {
        _phantom: PhantomData,
    };

    let ret = callback(&mut r);

    r.deregister_all();

    ret
}

// Could we allow users the creation of 'static RegistrationScopes? Like thread::spawn.
/// Lifetimed helper through which registrations can happen
///
/// For explanations of the `'env`' and `'id` lifetimes, see
/// [CountingThreadScope](crate::thread::CountingThreadScope) which has the same.
pub struct RegistrationScope<'env, 'id> {
    _phantom: PhantomData<(&'env (), &'id ())>,
}

impl<'env, 'id> RegistrationScope<'env, 'id> {
    /// Append a Gcoap listener in the global list of listeners, so that incoming requests are
    /// compared to the listener's match functions and, if matching, are run through its handlers.
    ///
    /// Note that the only provided way to get a suitable ListenerProvider is through
    /// [SingleHandlerListener].
    pub fn register<P>(&mut self, listener: &'env mut P)
    where
        // AsMut? hm, probably should re-consider the whole concept of the server ownign a mutable
        // reference to the resource. that makes simple server-mutable resources, but if they are
        // to do *anything* fro somewhere else, don't they need interior mutability anyway?
        P: 'env + ListenerProvider,
    {
        // Unsafe: Moving in a pointer to an internal structure to which we were given an exclusive
        // reference that outlives self -- and whoever can create a Self guarantees that
        // deregister_all() will be called before the end of this self's lifetime.
        unsafe { gcoap_register_listener(listener.get_listener() as *mut _) };
    }

    fn deregister_all(&mut self) {
        panic!("Registration callback returned, but Gcoap does not allow deregistration.");
    }
}

pub trait ListenerProvider {
    /// Provide an exclusive reference to the underlying gcoap listener. The function is marked
    /// unsafe as the returned value contains raw pointers that will later be dereferenced, and
    /// returning arbitrary pointers would make RegistratinScope::register() pass bad data on to C.
    unsafe fn get_listener<'a>(&'a mut self) -> &'a mut gcoap_listener_t;
}

/// A combination of the coap_resource_t and gcoap_listener_t structs with only a single resource
/// (Compared to many resources, this allows easier creation in Rust at the expense of larger
/// memory consumption and slower lookups in Gcoap).
///
/// A listener `l` can be hooked into the global Gcoap registry using [`scope`]`(|x| {
/// x.`[`register`](RegistrationScope::register)`(l) })`.
pub struct SingleHandlerListener<'a, H> {
    _phantom: PhantomData<&'a H>,
    resource: coap_resource_t,
    listener: gcoap_listener_t,
}

impl<'a, H> SingleHandlerListener<'a, H>
where
    H: 'a + Handler,
{
    // keeping methods u32 because the sys constants are too
    pub fn new(path: &'a cstr_core::CStr, methods: u32, handler: &'a mut H) -> Self {
        let methods = methods.try_into().unwrap();

        SingleHandlerListener {
            _phantom: PhantomData,
            resource: coap_resource_t {
                path: path.as_ptr(),
                handler: Some(Self::call_handler),
                methods: methods,
                context: handler as *mut _ as *mut c_void,
            },
            listener: gcoap_listener_t {
                resources: 0 as *const _,
                resources_len: 0,
                next: 0 as *mut _,
                // FIXME expose -- or tell people to write their own .wk/c, leave this NULL or even
                // no-op (which ain't NULL) and expose the encoding mechanism for extension in an
                // own .wk/c writer
                //
                // Works both for older versions without request_matcher and for current ones
                link_encoder: None,
                ..Default::default()
            },
        }
    }

    /// Create a listener whose single resource catches all requests and processes them through the
    /// handler.
    ///
    /// This is equivalent to a new single listener at "/" that takes all methods and matches on
    /// subtrees.
    ///
    /// Note that the taken Handler is a Gcoap [Handler] (which is there really only in case anyone
    /// wants extremely fine-grained control of what gcoap does); if you have a
    /// [coap_handler::Handler], you can wrap it in [crate::coap_handler::GcoapHandler] to for adaptation.
    pub fn new_catch_all(handler: &'a mut H) -> Self {
        Self::new(
            cstr_core::cstr!("/"),
            riot_sys::COAP_GET
                | riot_sys::COAP_POST
                | riot_sys::COAP_PUT
                | riot_sys::COAP_DELETE
                | riot_sys::COAP_FETCH
                | riot_sys::COAP_PATCH
                | riot_sys::COAP_IPATCH
                | riot_sys::COAP_MATCH_SUBTREE,
            handler,
        )
    }

    unsafe extern "C" fn call_handler(
        pkt: *mut coap_pkt_t,
        buf: *mut u8,
        len: u32,
        context: *mut c_void,
    ) -> i32 {
        let h = context as *mut H;
        let h = &mut *h;
        let mut pb = PacketBuffer {
            pkt,
            buf,
            len: len.try_into().unwrap(),
        };
        H::handle(h, &mut pb).try_into().unwrap()
    }
}

impl<'a, H> ListenerProvider for SingleHandlerListener<'a, H>
where
    H: 'a + Handler,
{
    unsafe fn get_listener(&mut self) -> &mut gcoap_listener_t {
        self.listener.resources = &self.resource;
        self.listener.resources_len = 1;
        self.listener.next = 0 as *mut _;

        &mut self.listener
    }
}

// Can be implemented by application code that'd then need to call some gcoap response functions,
// but preferably using the coap_handler module (behind the with-coap-handler feature).
pub trait Handler {
    fn handle(&mut self, pkt: &mut PacketBuffer) -> isize;
}

use riot_sys::{
    coap_get_total_hdr_len,
    coap_opt_add_opaque,
    coap_opt_add_uint,
    coap_opt_get_next,
    gcoap_register_listener,
    gcoap_resp_init,
};
#[deprecated(note = "Use direct riot_sys method codes instead")]
pub const GET: u32 = riot_sys::COAP_GET;

#[deprecated(note = "Use the coap_message abstractions")]
pub struct PayloadWriter<'a> {
    data: &'a mut [u8],
    cursor: usize,
}

#[allow(deprecated)] // still have to implement it while it's around
impl<'a> ::core::fmt::Write for PayloadWriter<'a> {
    fn write_str(&mut self, s: &str) -> ::core::fmt::Result {
        let mut s = s.as_bytes();
        let mut result = Ok(());
        if self.cursor + s.len() > self.data.len() {
            s = &s[..self.data.len() - self.cursor];
            result = Err(::core::fmt::Error);
        }
        self.data[self.cursor..self.cursor + s.len()].clone_from_slice(s);
        self.cursor += s.len();
        result
    }
}

/// A representation of the incoming or outgoing data on the server side of a request. This
/// includes the coap_pkt_t pre-parsed header and option pointers as well as the memory area
/// dedicated to returning the packet.
///
/// This struct wraps the unsafety of the C API, but does not structurally ensure that valid CoAP
/// messages are created. (For example, it does not keep the user from adding options after the
/// payload marker). Use CoAP generalization for that.
#[derive(Debug)]
pub struct PacketBuffer {
    pkt: *mut coap_pkt_t,
    buf: *mut u8,
    len: usize,
}

impl PacketBuffer {
    /// Wrapper for coap_get_code_raw
    pub fn get_code_raw(&self) -> u8 {
        (unsafe {
            riot_sys::coap_get_code_raw(
                self.pkt as *mut _, // missing const in C
            )
        }) as u8 // odd return type in C
    }

    /// Wrapper for coap_get_total_hdr_len
    fn get_total_hdr_len(&self) -> usize {
        (unsafe { coap_get_total_hdr_len(crate::inline_cast(self.pkt)) }) as usize
    }

    /// Wrapper for gcoap_resp_init
    ///
    /// As it is used and wrapped here, this makes GCOAP_RESP_OPTIONS_BUF bytes unusable, but
    /// working around that would mean duplicating code. Just set GCOAP_RESP_OPTIONS_BUF to zero to
    /// keep the overhead low.
    pub fn resp_init(&mut self, code: u8) -> Result<(), ()> {
        unsafe {
            gcoap_resp_init(
                self.pkt,
                self.buf,
                self.len.try_into().unwrap(),
                code.into(),
            )
        }
        .negative_to_error()
        .map_err(|_| ())?;
        Ok(())
    }

    pub fn set_code_raw(&mut self, code: u8) {
        unsafe { (*(*self.pkt).hdr).code = code };
    }

    /// Return the total number of bytes in the message, given that `payload_used` bytes were
    /// written at the payload pointer. Note that those bytes have to include the payload marker.
    ///
    /// This measures the distance between the payload pointer in the pkt and the start of the
    /// buffer. It is the header length after `prepare_response`, and grows as options are added.
    pub fn get_length(&self, payload_used: usize) -> usize {
        let own_length = unsafe { (*self.pkt).payload.offset_from(self.buf) };
        assert!(own_length >= 0);
        let total_length = own_length as usize + payload_used;
        assert!(total_length <= self.len.try_into().unwrap());
        total_length
    }

    /// A view of the current message payload
    ///
    /// This is only the CoAP payload after opt_finish has been called; before, it is a view on the
    /// remaining buffer space after any options that have already been added.
    pub fn payload(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts((*self.pkt).payload, (*self.pkt).payload_len as usize)
        }
    }

    /// A mutable view of the current message payload
    ///
    /// See `payload`.
    pub fn payload_mut(&mut self) -> &mut [u8] {
        unsafe {
            core::slice::from_raw_parts_mut((*self.pkt).payload, (*self.pkt).payload_len as usize)
        }
    }

    /// Add an integer value as an option
    pub fn opt_add_uint(&mut self, optnum: u16, value: u32) -> Result<(), ()> {
        unsafe { coap_opt_add_uint(self.pkt, optnum, value) }
            .negative_to_error()
            .map_err(|_| ())?;
        Ok(())
    }

    /// Add a binary value as an option
    pub fn opt_add_opaque(&mut self, optnum: u16, data: &[u8]) -> Result<(), ()> {
        unsafe {
            coap_opt_add_opaque(
                self.pkt,
                optnum,
                data.as_ptr(),
                data.len().try_into().unwrap(),
            )
        }
        .negative_to_error()
        .map_err(|_| ())?;
        Ok(())
    }

    pub fn opt_iter<'a>(&'a self) -> PacketBufferOptIter<'a> {
        PacketBufferOptIter {
            buffer: self,
            state: None,
        }
    }

    pub fn opt_iter_mut<'a>(&'a mut self) -> PacketBufferOptIterMut<'a> {
        PacketBufferOptIterMut {
            buffer: self,
            state: None,
        }
    }
}

pub struct PacketBufferOptIter<'a> {
    buffer: &'a PacketBuffer,
    state: Option<coap_optpos_t>,
}

impl<'a> Iterator for PacketBufferOptIter<'a> {
    type Item = (u16, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let size;
        let mut start = MaybeUninit::uninit();
        match &mut self.state {
            None => {
                let mut state = MaybeUninit::uninit();
                size = unsafe {
                    coap_opt_get_next(
                        &*self.buffer.pkt,
                        state.as_mut_ptr(),
                        start.as_mut_ptr(),
                        true,
                    )
                };
                if size < 0 {
                    return None;
                }
                // unsafe: as promised by coap_opt_get_next documentation
                self.state = Some(unsafe { state.assume_init() });
            }
            Some(ref mut state) => {
                size = unsafe {
                    coap_opt_get_next(&*self.buffer.pkt, state, start.as_mut_ptr(), false)
                };
                if size < 0 {
                    return None;
                }
            }
        }
        // unsafe: as promised by coap_opt_get_next documentation
        let start = unsafe { start.assume_init() };
        if start == 0 as *mut _ {
            None
        } else {
            // unsafe: that's the parts the coap_opt_get_next documentation promises, and we can
            // build an 'a-lived slice of it because we hold a &'a reference to the whole
            // PacketBuffer
            let slice = unsafe { core::slice::from_raw_parts(start, size as usize) };
            Some((self.state.unwrap().opt_num, slice))
        }
    }
}

pub struct PacketBufferOptIterMut<'a> {
    buffer: &'a mut PacketBuffer,
    state: Option<coap_optpos_t>,
}

impl<'a> Iterator for PacketBufferOptIterMut<'a> {
    type Item = (u16, &'a mut [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let size;
        let mut start = MaybeUninit::uninit();
        match &mut self.state {
            None => {
                let mut state = MaybeUninit::uninit();
                size = unsafe {
                    coap_opt_get_next(
                        &*self.buffer.pkt,
                        state.as_mut_ptr(),
                        start.as_mut_ptr(),
                        true,
                    )
                };
                if size < 0 {
                    return None;
                }
                // unsafe: as promised by coap_opt_get_next documentation
                self.state = Some(unsafe { state.assume_init() });
            }
            Some(ref mut state) => {
                size = unsafe {
                    coap_opt_get_next(&*self.buffer.pkt, state, start.as_mut_ptr(), false)
                };
                if size < 0 {
                    return None;
                }
            }
        }

        // unsafe: as promised by coap_opt_get_next documentation
        let start = unsafe { start.assume_init() };
        if start == 0 as *mut _ {
            None
        } else {
            // unsafe: that's the parts the coap_opt_get_next documentation promises, and we can
            // build an 'a-lived mutable slice of it because we hold a &'a mut reference to the
            // whole PacketBuffer, and the options do not overlap
            let slice = unsafe { core::slice::from_raw_parts_mut(start, size as usize) };
            Some((self.state.unwrap().opt_num, slice))
        }
    }
}
