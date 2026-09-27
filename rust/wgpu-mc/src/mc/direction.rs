use glam::{IVec3, ivec3};

static VECTOR: [IVec3; 6] = [
    ivec3(-1, 0, 0),
    ivec3(1, 0, 0),
    ivec3(0, -1, 0),
    ivec3(0, 1, 0),
    ivec3(0, 0, -1),
    ivec3(0, 0, 1),
];
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    West = 0,
    East = 1,
    Down = 2,
    Up = 3,
    North = 4,
    South = 5,
}
impl Direction {
    pub fn to_vec(&self) -> IVec3 {
        VECTOR[*self as usize]
    }
    pub fn opposite(&self) -> Self {
        match self {
            Self::West => Self::East,
            Self::East => Self::West,
            Self::Down => Self::Up,
            Self::Up => Self::Down,
            Self::North => Self::South,
            Self::South => Self::North,
        }
    }

    pub fn rotate(&self, vec: IVec3) -> IVec3 {
        let x = (match self {
            Direction::West => Direction::Down,
            Direction::East => Direction::Up,
            Direction::Down => Direction::East,
            Direction::Up => Direction::West,
            Direction::North => Direction::West,
            Direction::South => Direction::West,
        })
        .to_vec();
        let z = self.to_vec().cross(x);

        vec.x * x + vec.y * self.to_vec() + vec.z * z
    }
}

/// The bit a [`Direction`] has in the masks Minecraft computes for a block state.
///
/// The two enums are in different orders: Java's `Direction.ordinal()` is
/// `DOWN, UP, NORTH, SOUTH, WEST, EAST` and this one is `West, East, Down, Up, North, South`. A mask
/// that arrives from Java is a `1 << ordinal`, so it has to be read back through this table -
/// indexing it with `Direction as u8` puts every bit on the wrong face, which is a mistake that stays
/// invisible until a state occludes or hides faces *asymmetrically*: a full cube occludes all six, so
/// on stone every reading agrees.
///
/// Written as Java's ordinals in *this* crate's order, which is the order `Direction as u8` indexes.
const JAVA_ORDINAL: [u8; 6] = [
    4, // West
    5, // East
    0, // Down
    1, // Up
    2, // North
    3, // South
];

/// Whether `direction`'s bit is set in a mask the JVM computed from `Direction.ordinal()`.
pub fn java_mask_has(mask: u8, direction: Direction) -> bool {
    (mask >> JAVA_ORDINAL[direction as usize]) & 1 == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two orders are different, and a mask is only read correctly through the table.
    ///
    /// This is the whole reason `JAVA_ORDINAL` exists, so it is checked by name in both directions:
    /// a bit built from Java's ordinals lands on the face Java meant, and one built from this
    /// crate's own numbering does not.
    #[test]
    fn a_mask_that_came_from_java_is_read_in_java_s_order() {
        // Java: DOWN = 0, UP = 1, NORTH = 2, SOUTH = 3, WEST = 4, EAST = 5.
        let java_mask = (1 << 0) | (1 << 3);

        assert!(java_mask_has(java_mask, Direction::Down));
        assert!(java_mask_has(java_mask, Direction::South));

        for other in [
            Direction::Up,
            Direction::North,
            Direction::West,
            Direction::East,
        ] {
            assert!(
                !java_mask_has(java_mask, other),
                "{other:?} is not one of the two directions Java set"
            );
        }

        // The same mask read with this crate's own numbering is a different set of faces, which is
        // what reading it without the table would do.
        let misread = (1 << (Direction::Down as u8)) | (1 << (Direction::South as u8));
        assert_ne!(misread, java_mask, "the two orders have to differ here");
    }
}
