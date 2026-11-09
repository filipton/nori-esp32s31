//! Buffers in PSRAM by name: the heap's first region is internal RAM, which the BLE controller and
//! the radio need (the controller asserts on memory from PSRAM).

use alloc::vec::Vec;
use core::alloc::Layout;

/// `n` of `value` in PSRAM, for the program's life.
pub fn vec<T: Copy>(value: T, n: usize) -> Vec<T> {
    let layout = Layout::array::<T>(n).expect("a buffer's layout");
    let at = unsafe { esp_alloc::HEAP.alloc_caps(esp_alloc::MemoryCapability::External.into(), layout) }.cast::<T>();
    assert!(!at.is_null(), "out of PSRAM");
    for i in 0..n {
        unsafe { at.add(i).write(value) };
    }
    // The global allocator frees from the same heap.
    unsafe { Vec::from_raw_parts(at, n, n) }
}

/// `n` of `value` in PSRAM, or none if it has no room (to be dropped after use).
pub fn try_vec<T: Copy>(value: T, n: usize) -> Option<Vec<T>> {
    let layout = Layout::array::<T>(n).ok()?;
    let at = unsafe { esp_alloc::HEAP.alloc_caps(esp_alloc::MemoryCapability::External.into(), layout) }.cast::<T>();
    if at.is_null() {
        return None;
    }
    for i in 0..n {
        unsafe { at.add(i).write(value) };
    }
    Some(unsafe { Vec::from_raw_parts(at, n, n) })
}

