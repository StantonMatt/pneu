//! Host services used by the headless daemon. Desktop UI effects live in PcActions.
#[cfg(target_os = "macos")]
use anyhow::ensure;
use anyhow::{Context, Result};
use std::{
    env,
    path::{Path, PathBuf},
};
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux::*;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(crate) use macos::*;

pub(crate) mod fs;
pub(crate) use fs::private;

#[cfg(any(target_os = "macos", test))]
pub mod macos_login;
