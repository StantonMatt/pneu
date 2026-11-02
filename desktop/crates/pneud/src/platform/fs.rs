//! Descriptor-relative filesystem operations used by shared daemon code.
//! Paths returned by Folder::path are display/recovery hints, never mutation authority.
//! Backends own root/prefix traversal, no-follow opens, private creation and identity.
//! Identity and Version are opaque so callers do not depend on Unix inode layouts.
#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::*;

#[cfg(all(test, unix))]
mod tests;

#[derive(Clone, Copy)]
pub(crate) enum CaptureType {
    File,
    Directory,
    #[cfg_attr(windows, allow(dead_code))]
    Link,
    Special,
}
