use std::{fmt::Debug, ops::Range};
#[macro_export]
macro_rules! span {
    ($l: pat, $p: pat) => {
        Spanned {
            value: $p,
            location: $l,
        }
    };
    ($p: pat) => {
        Spanned { value: $p, .. }
    };
}
#[derive(Debug, Clone, PartialEq)]
pub struct Location {
    pub range: Range<usize>,
    pub file_id: usize,
}
#[derive(Clone)]
pub struct Spanned<T> {
    pub value: T,
    pub location: Location,
}

impl Location {
    pub fn new() -> Self {
        Self {
            range: 0..0,
            file_id: 0,
        }
    }
    pub fn until(&self, other: &Self) -> Self {
        // if self.file_id == other.file_id && self.range.start < other.range.end {
        //     panic!("WHAT");
        // }
        // debug_assert!(self.range.start < other.range.end);
        // assert_eq!(self.file_id, other.file_id);
        Self {
            range: self.range.start..other.range.end,
            file_id: self.file_id,
        }
    }
    pub fn until_vec<T>(&self, other: &Vec<Spanned<T>>) -> Self {
        let end = other.last().map(|t| &t.location).unwrap_or(self);
        self.until(end)
    }
    pub fn as_fallback_for_vec<T>(&self, other: &Vec<Spanned<T>>) -> Self {
        let first = other
            .first()
            .map(|s| s.location.clone())
            .unwrap_or(self.clone());
        let last = other
            .first()
            .map(|s| s.location.clone())
            .unwrap_or(self.clone());

        first.until(&last)
    }
    pub fn into_spanned<T>(self, value: T) -> Spanned<T> {
        Spanned {
            value,
            location: self,
        }
    }
    pub fn to_spanned<T>(&self, value: T) -> Spanned<T> {
        Spanned {
            value,
            location: self.clone(),
        }
    }
}

impl<T> Spanned<T> {
    pub fn new(location: Location, value: T) -> Spanned<T> {
        Spanned {
            value: value,
            location,
        }
    }
    pub fn map<U, F: FnMut(T) -> U>(self, mut f: F) -> Spanned<U> {
        Spanned {
            location: self.location.clone(),
            value: f(self.value),
        }
    }
    pub fn map_ref<U: Clone, F: Fn(&T) -> U>(&self, f: F) -> Spanned<U> {
        Spanned {
            location: self.location.clone(),
            value: f(&self.value),
        }
    }
    pub fn with_offset(self, offset: usize) -> Spanned<T> {
        Spanned {
            value: self.value,
            location: Location {
                range: (self.location.range.start + offset)..(self.location.range.end + offset),
                file_id: self.location.file_id,
            }, // location:
        }
    }
    pub fn start(&self) -> usize {
        self.location.range.start
    }
    pub fn end(&self) -> usize {
        self.location.range.end
    }
}
impl<T: PartialEq> PartialEq for Spanned<T> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}
impl<T: Debug> Debug for Spanned<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if f.alternate() {
            write!(f, "{:#?}", self.value)
        } else {
            write!(f, "{:?}", self.value)
        }
    }
}
