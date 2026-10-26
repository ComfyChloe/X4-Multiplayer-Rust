//! Minimal Lua 5.1 / LuaJIT UDP module.
//!
//! This is the Rust replacement for `native/net.c`. It exposes the same API:
//!
//! ```lua
//! local net = package.loadlib(path, "luaopen_x4lan_net")()
//! local socket = net.open("192.168.1.10", 45841)
//! socket:send(payload)
//! local reply = socket:receive()
//! socket:close()
//! ```
//!
//! # Why a native module at all
//!
//! X4's "Protected UI" mode blocks the standard Lua socket library, and the
//! game's LuaJIT build has no socket support compiled in. The extension
//! therefore loads a small shared library through `package.loadlib`, which
//! requires Protected UI mode to be disabled.
//!
//! # Dynamic symbol resolution
//!
//! The module is loaded *into* the game process, and the game already has a
//! LuaJIT runtime in memory. The module must not link against a Lua library:
//! it would need its own copy, and the two would disagree about the
//! interpreter state. So every Lua function is resolved from the already
//! loaded runtime at load time, exactly as the C original does.
//!
//! LuaJIT is ABI-compatible with Lua 5.1, so the 5.1 C API is the correct
//! interface.

#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

use std::ffi::{c_char, c_double, c_int, c_void, CStr, CString};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};

/// The opaque Lua interpreter state.
#[repr(C)]
pub struct lua_State;

/// A Lua C function.
pub type lua_CFunction = Option<unsafe extern "C" fn(lua_State: *mut lua_State) -> c_int>;

// ---------------------------------------------------------------------------
// Dynamically resolved Lua API
// ---------------------------------------------------------------------------

/// The Lua functions this module uses, resolved from the running interpreter.
struct LuaApi {
    checklstring: unsafe extern "C" fn(*mut lua_State, c_int, *mut usize) -> *const c_char,
    checknumber: unsafe extern "C" fn(*mut lua_State, c_int) -> c_double,
    checkudata: unsafe extern "C" fn(*mut lua_State, c_int, *const c_char) -> *mut c_void,
    newmetatable: unsafe extern "C" fn(*mut lua_State, *const c_char) -> c_int,
    newuserdata: unsafe extern "C" fn(*mut lua_State, usize) -> *mut c_void,
    pushboolean: unsafe extern "C" fn(*mut lua_State, c_int),
    pushnil: unsafe extern "C" fn(*mut lua_State),
    pushlstring: unsafe extern "C" fn(*mut lua_State, *const c_char, usize),
    pushcclosure: unsafe extern "C" fn(*mut lua_State, lua_CFunction, c_int),
    pushvalue: unsafe extern "C" fn(*mut lua_State, c_int),
    setfield: unsafe extern "C" fn(*mut lua_State, c_int, *const c_char),
    getfield: unsafe extern "C" fn(*mut lua_State, c_int, *const c_char),
    setmetatable: unsafe extern "C" fn(*mut lua_State, c_int) -> c_int,
    createtable: unsafe extern "C" fn(*mut lua_State, c_int, c_int),
    settop: unsafe extern "C" fn(*mut lua_State, c_int),
    error: unsafe extern "C" fn(*mut lua_State, c_int) -> c_int,
}

/// The resolved API, populated once when the module loads.
static LUA_API: std::sync::OnceLock<LuaApi> = std::sync::OnceLock::new();

/// A symbol name, NUL-terminated.
///
/// `GetProcAddress` and `dlsym` both read a C string. Passing a Rust byte
/// slice without its terminator makes them read past the end of the slice
/// and return a garbage address, which then crashes the host process when
/// the function is called.
const fn symbol_name(name: &'static [u8]) -> &'static CStr {
    // The names below are all NUL-terminated by construction.
    unsafe { CStr::from_bytes_with_nul_unchecked(name) }
}

/// The Lua symbols this module needs, as NUL-terminated names.
const SYMBOLS: [&[u8]; 17] = [
    b"luaL_checklstring\0",
    b"luaL_checknumber\0",
    b"luaL_checkudata\0",
    b"luaL_newmetatable\0",
    b"lua_newuserdata\0",
    b"lua_pushboolean\0",
    b"lua_pushnil\0",
    b"lua_pushlstring\0",
    b"lua_pushcclosure\0",
    b"lua_pushvalue\0",
    b"lua_setfield\0",
    b"lua_getfield\0",
    b"lua_setmetatable\0",
    b"lua_createtable\0",
    b"lua_settop\0",
    b"lua_error\0",
    b"lua_gettop\0",
];

/// Resolves a symbol from the running Lua runtime.
#[cfg(windows)]
unsafe fn resolve(name: &CStr) -> Option<*mut c_void> {
    use std::ffi::c_void as void;

    extern "system" {
        fn GetModuleHandleA(name: *const c_char) -> *mut void;
        fn GetProcAddress(module: *mut void, name: *const c_char) -> *mut void;
        fn K32EnumProcessModules(
            process: *mut void,
            modules: *mut *mut void,
            size: u32,
            needed: *mut u32,
        ) -> c_int;
        fn GetCurrentProcess() -> *mut void;
    }

    let mut module = GetModuleHandleA(c"lua51_64.dll".as_ptr());
    if module.is_null() {
        module = GetModuleHandleA(std::ptr::null());
    }
    if module.is_null() {
        return None;
    }
    let pointer = GetProcAddress(module, name.as_ptr());
    if !pointer.is_null() {
        return Some(pointer);
    }

    // The runtime may live in a differently named module. Scan the process's
    // modules for one that exports the symbol.
    let mut modules: Vec<*mut void> = Vec::new();
    let mut needed: u32 = 0;
    let process = GetCurrentProcess();
    if K32EnumProcessModules(process, std::ptr::null_mut(), 0, &mut needed) == 0 && needed > 0 {
        let count = (needed as usize) / std::mem::size_of::<*mut void>();
        modules.resize(count, std::ptr::null_mut());
        if K32EnumProcessModules(process, modules.as_mut_ptr(), needed, &mut needed) != 0 {
            for candidate in modules {
                if candidate.is_null() {
                    continue;
                }
                let pointer = GetProcAddress(candidate, name.as_ptr());
                if !pointer.is_null() {
                    return Some(pointer);
                }
            }
        }
    }
    None
}

/// Resolves a symbol from the running Lua runtime.
#[cfg(unix)]
unsafe fn resolve(name: &CStr) -> Option<*mut c_void> {
    use std::os::raw::c_char as raw_char;

    extern "C" {
        fn dlsym(handle: *mut c_void, symbol: *const raw_char) -> *mut c_void;
    }
    const RTLD_DEFAULT: *mut c_void = 0 as *mut c_void;
    Some(dlsym(RTLD_DEFAULT, name.as_ptr()))
}

/// Loads the Lua API, or returns false if the runtime is not present.
///
/// The API is resolved once and kept. A second call must succeed rather
/// than fail, because the module may legitimately be loaded more than once
/// in a process: `package.loadlib` calls the entry point afresh for every
/// new Lua state, and each call has to hand back a working module.
unsafe fn load_lua_api() -> bool {
    // Resolve every symbol before touching the cache, so a partial
    // resolution cannot leave the module half-initialised.
    let mut pointers: [*mut c_void; SYMBOLS.len()] = [std::ptr::null_mut(); SYMBOLS.len()];
    for (index, name) in SYMBOLS.iter().enumerate() {
        match resolve(symbol_name(name)) {
            Some(pointer) => pointers[index] = pointer,
            None => return false,
        }
    }

    LUA_API.get_or_init(|| {
        macro_rules! symbol {
            ($index:literal, $ty:ty) => {
                std::mem::transmute::<*mut c_void, $ty>(pointers[$index])
            };
        }
        LuaApi {
            checklstring: symbol!(
                0,
                unsafe extern "C" fn(*mut lua_State, c_int, *mut usize) -> *const c_char
            ),
            checknumber: symbol!(1, unsafe extern "C" fn(*mut lua_State, c_int) -> c_double),
            checkudata: symbol!(
                2,
                unsafe extern "C" fn(*mut lua_State, c_int, *const c_char) -> *mut c_void
            ),
            newmetatable: symbol!(
                3,
                unsafe extern "C" fn(*mut lua_State, *const c_char) -> c_int
            ),
            newuserdata: symbol!(
                4,
                unsafe extern "C" fn(*mut lua_State, usize) -> *mut c_void
            ),
            pushboolean: symbol!(5, unsafe extern "C" fn(*mut lua_State, c_int)),
            pushnil: symbol!(6, unsafe extern "C" fn(*mut lua_State)),
            pushlstring: symbol!(
                7,
                unsafe extern "C" fn(*mut lua_State, *const c_char, usize)
            ),
            pushcclosure: symbol!(
                8,
                unsafe extern "C" fn(*mut lua_State, lua_CFunction, c_int)
            ),
            pushvalue: symbol!(9, unsafe extern "C" fn(*mut lua_State, c_int)),
            setfield: symbol!(
                10,
                unsafe extern "C" fn(*mut lua_State, c_int, *const c_char)
            ),
            getfield: symbol!(
                11,
                unsafe extern "C" fn(*mut lua_State, c_int, *const c_char)
            ),
            setmetatable: symbol!(12, unsafe extern "C" fn(*mut lua_State, c_int) -> c_int),
            createtable: symbol!(13, unsafe extern "C" fn(*mut lua_State, c_int, c_int)),
            settop: symbol!(14, unsafe extern "C" fn(*mut lua_State, c_int)),
            error: symbol!(15, unsafe extern "C" fn(*mut lua_State, c_int) -> c_int),
        }
    });
    true
}

/// Returns the resolved Lua API.
///
/// # Safety
/// Only callable once the module has been loaded.
fn api() -> &'static LuaApi {
    LUA_API.get().expect("the Lua API is resolved at load time")
}

// ---------------------------------------------------------------------------
// Module state
// ---------------------------------------------------------------------------

/// The registry pseudo-index, where Lua keeps its metatables.
const REGISTRYINDEX: c_int = -10000;

/// The metatable name for a socket userdata.
const METATABLE: &[u8] = b"x4lan.udp.v1\0";

/// The largest packet the module will send or receive.
const MAX_PACKET: usize = 768;

/// A socket, stored as Lua userdata.
#[repr(C)]
struct Connection {
    /// The socket handle.
    handle: c_int,
    /// Set once the socket has been closed, so `__gc` is idempotent.
    closed: bool,
}

// ---------------------------------------------------------------------------
// Socket helpers
// ---------------------------------------------------------------------------

/// Creates and connects a non-blocking UDP socket.
fn open_socket(host: &str, port: u16) -> Result<c_int, String> {
    let address: Ipv4Addr = host
        .parse()
        .map_err(|_| "invalid IPv4 address".to_string())?;
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .map_err(|error| format!("could not bind a local socket: {error}"))?;
    socket
        .connect(SocketAddr::from((address, port)))
        .map_err(|error| format!("could not connect: {error}"))?;
    socket
        .set_nonblocking(true)
        .map_err(|error| format!("could not set the socket non-blocking: {error}"))?;
    Ok(raw_handle(socket))
}

/// Takes ownership of the socket's handle.
#[cfg(unix)]
fn raw_handle(socket: UdpSocket) -> c_int {
    use std::os::fd::IntoRawFd;
    socket.into_raw_fd()
}

/// Takes ownership of the socket's handle.
#[cfg(windows)]
fn raw_handle(socket: UdpSocket) -> c_int {
    use std::os::windows::io::IntoRawSocket;
    socket.into_raw_socket() as c_int
}

/// Closes a socket if it is still open.
unsafe fn close_socket(connection: &mut Connection) {
    if !connection.closed && connection.handle >= 0 {
        close_raw(connection.handle);
        connection.handle = -1;
        connection.closed = true;
    }
}

/// Closes a socket handle.
#[cfg(unix)]
unsafe fn close_raw(handle: c_int) {
    extern "C" {
        fn close(handle: c_int) -> c_int;
    }
    close(handle);
}

/// Closes a socket handle.
#[cfg(windows)]
unsafe fn close_raw(handle: c_int) {
    extern "system" {
        fn closesocket(socket: usize) -> c_int;
    }
    closesocket(handle as usize);
}

// ---------------------------------------------------------------------------
// Lua bindings
// ---------------------------------------------------------------------------

/// Raises a Lua error with a fixed message.
///
/// The message is pushed as a string and `lua_error` is called, rather than
/// using `luaL_error`, because `luaL_error` treats its argument as a
/// `printf` format string. A message containing a `%` would then be read as
/// a format specification.
unsafe fn raise(L: *mut lua_State, message: &str) {
    let lua = api();
    let text = CString::new(message).unwrap_or_else(|_| CString::new("error").expect("literal"));
    (lua.pushlstring)(L, text.as_ptr(), text.as_bytes().len());
    (lua.error)(L, 1);
}

/// `socket:send(payload) -> boolean`
///
/// Sends the whole payload, or returns false if the socket is closed or the
/// payload exceeds the transport limit.
unsafe extern "C" fn net_send(L: *mut lua_State) -> c_int {
    let lua = api();
    let connection = (lua.checkudata)(L, 1, METATABLE.as_ptr() as *const c_char) as *mut Connection;
    let mut length: usize = 0;
    let payload = (lua.checklstring)(L, 2, &mut length);
    if connection.is_null() || (*connection).handle < 0 || length > MAX_PACKET {
        (lua.pushboolean)(L, 0);
        return 1;
    }
    let bytes = std::slice::from_raw_parts(payload as *const u8, length);
    let sent = send_bytes((*connection).handle, bytes);
    (lua.pushboolean)(L, if sent == length as isize { 1 } else { 0 });
    1
}

/// `socket:receive() -> string | nil`
///
/// Returns the next datagram, or nil when nothing is waiting.
unsafe extern "C" fn net_receive(L: *mut lua_State) -> c_int {
    let lua = api();
    let connection = (lua.checkudata)(L, 1, METATABLE.as_ptr() as *const c_char) as *mut Connection;
    if connection.is_null() || (*connection).handle < 0 {
        (lua.pushnil)(L);
        return 1;
    }
    let mut buffer = [0u8; MAX_PACKET + 1];
    let received = receive_bytes((*connection).handle, &mut buffer);
    if received <= 0 || received > MAX_PACKET as isize {
        (lua.pushnil)(L);
        return 1;
    }
    (lua.pushlstring)(L, buffer.as_ptr() as *const c_char, received as usize);
    1
}

/// `socket:close()`
unsafe extern "C" fn net_close(L: *mut lua_State) -> c_int {
    let lua = api();
    let connection = (lua.checkudata)(L, 1, METATABLE.as_ptr() as *const c_char) as *mut Connection;
    if !connection.is_null() {
        close_socket(&mut *connection);
    }
    0
}

/// The `__gc` metamethod, so an abandoned socket is closed by the collector.
unsafe extern "C" fn net_gc(L: *mut lua_State) -> c_int {
    let lua = api();
    let connection = (lua.checkudata)(L, 1, METATABLE.as_ptr() as *const c_char) as *mut Connection;
    if !connection.is_null() {
        close_socket(&mut *connection);
    }
    0
}

/// `net.open(host, port) -> socket`
unsafe extern "C" fn net_open(L: *mut lua_State) -> c_int {
    let lua = api();
    let mut length: usize = 0;
    let host = (lua.checklstring)(L, 1, &mut length);
    let port = (lua.checknumber)(L, 2);
    if !(1.0..=65535.0).contains(&port) || port != (port as i32) as f64 {
        raise(L, "UDP port must be an integer from 1 to 65535");
        return 0;
    }
    if length == 0 || length >= 16 {
        raise(L, "host must be a numeric IPv4 address");
        return 0;
    }
    let Ok(host_text) = CStr::from_ptr(host).to_str() else {
        raise(L, "host must be a numeric IPv4 address");
        return 0;
    };
    if host_text.len() != length {
        raise(L, "host must be a numeric IPv4 address");
        return 0;
    }

    let handle = match open_socket(host_text, port as u16) {
        Ok(handle) => handle,
        Err(message) => {
            raise(L, &message);
            return 0;
        }
    };

    let userdata = (lua.newuserdata)(L, std::mem::size_of::<Connection>()) as *mut Connection;
    (*userdata).handle = handle;
    (*userdata).closed = false;

    (lua.getfield)(L, REGISTRYINDEX, METATABLE.as_ptr() as *const c_char);
    (lua.setmetatable)(L, -2);
    1
}

/// Registers a function in a table.
unsafe fn method(L: *mut lua_State, name: &[u8], function: lua_CFunction) {
    let lua = api();
    (lua.pushcclosure)(L, function, 0);
    (lua.setfield)(L, -2, name.as_ptr() as *const c_char);
}

/// The module entry point Lua looks for: `luaopen_x4lan_net`.
///
/// # Safety
/// Called by Lua with a valid state pointer.
#[no_mangle]
pub unsafe extern "C" fn luaopen_x4lan_net(L: *mut lua_State) -> c_int {
    if !load_lua_api() {
        return 0;
    }
    let lua = api();
    (lua.newmetatable)(L, METATABLE.as_ptr() as *const c_char);
    (lua.pushvalue)(L, -1);
    (lua.setfield)(L, -2, b"__index\0".as_ptr() as *const c_char);
    method(L, b"send\0", Some(net_send));
    method(L, b"receive\0", Some(net_receive));
    method(L, b"close\0", Some(net_close));
    method(L, b"__gc\0", Some(net_gc));
    (lua.settop)(L, -2);

    (lua.createtable)(L, 0, 1);
    method(L, b"open\0", Some(net_open));
    1
}

// ---------------------------------------------------------------------------
// Platform socket calls
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn send_bytes(handle: c_int, bytes: &[u8]) -> isize {
    extern "C" {
        fn send(handle: c_int, buf: *const u8, len: usize, flags: c_int) -> isize;
    }
    unsafe { send(handle, bytes.as_ptr(), bytes.len(), 0) }
}

#[cfg(unix)]
fn receive_bytes(handle: c_int, buffer: &mut [u8]) -> isize {
    extern "C" {
        fn recv(handle: c_int, buf: *mut u8, len: usize, flags: c_int) -> isize;
    }
    unsafe { recv(handle, buffer.as_mut_ptr(), buffer.len(), 0) }
}

#[cfg(windows)]
fn send_bytes(handle: c_int, bytes: &[u8]) -> isize {
    extern "system" {
        fn send(socket: usize, buf: *const u8, len: c_int, flags: c_int) -> c_int;
    }
    unsafe { send(handle as usize, bytes.as_ptr(), bytes.len() as c_int, 0) as isize }
}

#[cfg(windows)]
fn receive_bytes(handle: c_int, buffer: &mut [u8]) -> isize {
    extern "system" {
        fn recv(socket: usize, buf: *mut u8, len: c_int, flags: c_int) -> c_int;
    }
    unsafe {
        recv(
            handle as usize,
            buffer.as_mut_ptr(),
            buffer.len() as c_int,
            0,
        ) as isize
    }
}
