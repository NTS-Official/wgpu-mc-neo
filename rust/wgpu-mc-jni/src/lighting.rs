use std::fmt::Debug;

/// A decoded pair of Minecraft lightmaps, handed over by `registerBlockState`'s neighbours on the
/// JVM side.
///
/// Nothing on this side reads a lightmap as a pair of arrays yet - the light that arrives comes with
/// the section payload instead (see `SectionLight`) - so this is the shape the JVM side writes
/// rather than one with a reader. Kept for the same reason `alloc.rs`'s exports are: it is the
/// other end of a handover that is written down on both sides.
#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeserializedLightData {
    pub sky_light: Box<[u8; 2048]>,
    pub block_light: Box<[u8; 2048]>,
}
