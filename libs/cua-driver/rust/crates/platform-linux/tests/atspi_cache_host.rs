//! Run the actual snapshot cache/registry tests without Linux GUI dependencies.
#![cfg(target_os = "macos")]
#![allow(dead_code)]

#[path = "../src/atspi/types.rs"]
mod types;
use types::{AtspiIdentity, AtspiNode};
#[path = "../src/atspi/cache.rs"]
mod cache;
#[path = "../src/atspi/identity.rs"]
mod identity;
