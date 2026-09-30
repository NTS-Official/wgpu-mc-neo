#![doc = include_str!("../README.md")]
#![warn(missing_docs)]
// **Two lints this crate has always failed, allowed here rather than fixed in it.**
//
// The copy in `rust/minecraft-assets` exists for one added field - a model face's `shade` flag - and
// nothing else about it is ours to rewrite: the two below are upstream's own style, they are not warnings
// about anything this renderer can observe, and editing the bodies of a vendored crate is how a copy stops
// being one. The workspace builds with `-D warnings`, so without this the whole build fails on code that
// has nothing to do with the change that made the copy necessary.
//
// A third - a redundant `&` in one `write!` - *was* fixed rather than allowed, because clippy is right
// about it and the edit cannot change behaviour. See `api::resource::identifier`.
//
// They are named one by one rather than switching linting off for the crate, so a genuine warning here is
// still a warning.
#![allow(clippy::large_enum_variant, clippy::derived_hash_with_manual_eq)]

pub mod api;
pub mod schemas;
pub mod versions;
