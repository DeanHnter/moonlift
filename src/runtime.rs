// This file defines external helper functions used by the JIT.

#[no_mangle]
pub extern "C" fn lua_len(val: i64) -> i64 {
    // Dummy implementation: returns 0.
    0
}

#[no_mangle]
pub extern "C" fn lua_newtable() -> i64 {
    // Dummy implementation: returns 0 as a pointer.
    0
}

#[no_mangle]
pub extern "C" fn lua_settable(_table: i64, _index: i64, _value: i64) -> i64 {
    // Dummy implementation: returns 0.
    0
}

#[no_mangle]
pub extern "C" fn lua_concat(_a: i64, _b: i64) -> i64 {
    // Dummy implementation: returns 0.
    0
}

#[no_mangle]
pub extern "C" fn lua_next(_table: i64, _key: i64) -> i64 {
    // Dummy implementation: returns 0 to indicate end of iteration.
    0
}

#[no_mangle]
pub extern "C" fn lua_getglobal(_name_ptr: *const u8) -> i64 {
    // Dummy implementation: returns 0.
    0
}

#[no_mangle]
pub extern "C" fn lua_setglobal(_name_ptr: *const u8, _value: i64) -> i64 {
    // Dummy implementation: returns 0.
    0
}

#[no_mangle]
pub extern "C" fn lua_tonumber(_val: i64) -> f64 {
    // Dummy implementation: returns 0.0.
    0.0
}

#[no_mangle]
pub extern "C" fn lua_tostring(_val: i64) -> *const u8 {
    // Dummy implementation: returns a null pointer.
    std::ptr::null()
}

// Added new helper for table indexing (reading from a table)
#[no_mangle]
pub extern "C" fn lua_gettable(_table: i64, _index: i64) -> i64 {
    // Dummy implementation: returns 0.
    0
} 