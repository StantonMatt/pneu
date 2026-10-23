//! Host services used by the headless daemon. Desktop UI effects live in PcActions.
use anyhow::{Context, Result, ensure};
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

pub(crate) mod private;
