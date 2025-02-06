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