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

use std::ffi::{c_char, c_double, c_int, c_void, CStr};
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
    error: unsafe extern "C" fn(*mut lua_State, *const c_char, ...) -> c_int,
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
}

/// The resolved API, populated once when the module loads.
static mut LUA_API: Option<LuaApi> = None;

/// Resolves a symbol from the running Lua runtime.
#[cfg(windows)]
unsafe fn resolve(name: &[u8]) -> Option<*mut c_void> {
    use std::ffi::c_void as void;
    type GetModuleHandleA = unsafe extern "system" fn(*const c_char) -> *mut void;
    type GetProcAddress = unsafe extern "system" fn(*mut void, *const c_char) -> *mut void;

    extern "system" {
        fn GetModuleHandleA(name: *const c_char) -> *mut void;
        fn GetProcAddress(module: *mut void, name: *const c_char) -> *mut void;
    }
    let _ = (GetModuleHandleA as GetModuleHandleA, GetProcAddress as GetProcAddress);

    // The game ships LuaJIT as `lua51_64.dll` on Windows.
    let mut module = GetModuleHandleA(b"lua51_64.dll\0".as_ptr() as *const c_char);
    if module.is_null() {
        module = GetModuleHandleA(std::ptr::null());
    }
    if module.is_null() {
        return None;
    }
    Some(GetProcAddress(module, name.as_ptr() as *const c_char))
}

/// Resolves a symbol from the running Lua runtime.
#[cfg(unix)]
unsafe fn resolve(name: &[u8]) -> Option<*mut c_void> {
    use std::os::raw::c_char as raw_char;

    extern "C" {
        fn dlsym(handle: *mut c_void, symbol: *const raw_char) -> *mut c_void;
    }
    const RTLD_DEFAULT: *mut c_void = 0 as *mut c_void;
    Some(dlsym(RTLD_DEFAULT, name.as_ptr() as *const raw_char))
}

/// Loads the Lua API, or returns false if the runtime is not present.
unsafe fn load_lua_api() -> bool {
    macro_rules! symbol {
        ($name:literal) => {
            match resolve($name.as_bytes()) {
                Some(pointer) => std::mem::transmute(pointer),
                None => return false,
            }
        };
    }
    let api = LuaApi {
        checklstring: symbol!("luaL_checklstring"),
        checknumber: symbol!("luaL_checknumber"),
        checkudata: symbol!("luaL_checkudata"),
        newmetatable: symbol!("luaL_newmetatable"),
        error: symbol!("luaL_error"),
        newuserdata: symbol!("lua_newuserdata"),
        pushboolean: symbol!("lua_pushboolean"),
        pushnil: symbol!("lua_pushnil"),
        pushlstring: symbol!("lua_pushlstring"),
        pushcclosure: symbol!("lua_pushcclosure"),
        pushvalue: symbol!("lua_pushvalue"),
        setfield: symbol!("lua_setfield"),
        getfield: symbol!("lua_getfield"),
        setmetatable: symbol!("lua_setmetatable"),
        createtable: symbol!("lua_createtable"),
        settop: symbol!("lua_settop"),
    };
    LUA_API = Some(api);
    true
}

/// Returns the resolved Lua API.
///
/// # Safety
/// Only callable once the module has been loaded.
unsafe fn api() -> &'static LuaApi {
    LUA_API.as_ref().expect("the Lua API is resolved at load time")
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
    let address: Ipv4Addr = host.parse().map_err(|_| "invalid IPv4 address".to_string())?;
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
    if !(port >= 1.0 && port <= 65535.0) || port != (port as i32) as f64 {
        (lua.error)(
            L,
            b"UDP port must be an integer from 1 to 65535\0".as_ptr() as *const c_char,
        );
        return 0;
    }
    if length == 0 || length >= 16 {
        (lua.error)(
            L,
            b"host must be a numeric IPv4 address\0".as_ptr() as *const c_char,
        );
        return 0;
    }
    let Ok(host_text) = CStr::from_ptr(host).to_str() else {
        (lua.error)(
            L,
            b"host must be a numeric IPv4 address\0".as_ptr() as *const c_char,
        );
        return 0;
    };
    if host_text.len() != length {
        (lua.error)(
            L,
            b"host must be a numeric IPv4 address\0".as_ptr() as *const c_char,
        );
        return 0;
    }

    let handle = match open_socket(host_text, port as u16) {
        Ok(handle) => handle,
        Err(message) => {
            let message = format!("{message}\0");
            (lua.error)(L, message.as_ptr() as *const c_char);
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
fn send_bytes(handle: cint_type, bytes: &[u8]) -> isize {
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
    unsafe { recv(handle as usize, buffer.as_mut_ptr(), buffer.len() as c_int, 0) as isize }
}

/// The Windows socket handle type.
#[cfg(windows)]
type cint_type = c_int;
