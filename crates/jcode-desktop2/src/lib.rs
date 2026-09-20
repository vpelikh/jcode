//! Reloadable desktop2 application worker.
//!
//! The binary and this shared library intentionally compile the same current
//! Scene/Model/App implementation. The binary owns the native host. Self-dev
//! activation loads this library and swaps only the application callbacks.
#![allow(dead_code)] // The lib is the self-dev worker; the bin-owned entry/render paths it shares appear dead only here.

include!("main.rs");
