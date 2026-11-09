//! The firmware's modules that need no chip, as they are in `../src`.

// The firmware is a binary: its `new`s need no `Default`.
#![allow(clippy::new_without_default)]

extern crate alloc;

#[path = "../../src/flac.rs"]
pub mod flac;
#[path = "../../src/light.rs"]
pub mod light;
#[path = "../../src/stream.rs"]
pub mod stream;
