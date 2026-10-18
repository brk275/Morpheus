//! Compile the production Slint interface without the desktop runtime or Matrix.
//!
//! The build script uses the same pinned compiler and source as the app. This
//! catches interface compilation failures before the full desktop build; it
//! does not replace generated-UI, native platform or installed-app testing.
