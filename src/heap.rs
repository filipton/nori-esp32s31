//! The heap, as esp-alloc's but by core: what the second core asks for (the screens: a cover's decoding,
//! its buffers) comes from PSRAM, keeping internal RAM for the first core's radio, which needs it.

use core::alloc::{GlobalAlloc, Layout};

use esp_alloc::MemoryCapability;
use esp_hal::system::Cpu;

struct ByCore;

unsafe impl GlobalAlloc for ByCore {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if Cpu::current() == Cpu::AppCpu {
            let at = unsafe { esp_alloc::HEAP.alloc_caps(MemoryCapability::External.into(), layout) };
            if !at.is_null() {
                return at;
            }
        }
        unsafe { esp_alloc::HEAP.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { esp_alloc::HEAP.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: ByCore = ByCore;
