use std::ops::{Index, IndexMut};

#[cfg(feature = "serde")]
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::vte::ansi::{NamedColor, Rgb};

/// Number of terminal colors.
pub const COUNT: usize = 269;

/// Array of indexed colors.
///
/// | Indices  | Description       |
/// | -------- | ----------------- |
/// | 0..16    | Named ANSI colors |
/// | 16..232  | Color cube        |
/// | 233..256 | Grayscale ramp    |
/// | 256      | Foreground        |
/// | 257      | Background        |
/// | 258      | Cursor            |
/// | 259..267 | Dim colors        |
/// | 267      | Bright foreground |
/// | 268      | Dim background    |
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Colors([Option<Rgb>; COUNT]);

#[cfg(feature = "serde")]
impl Serialize for Colors {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.as_slice().serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de> Deserialize<'de> for Colors {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let values = Vec::<Option<Rgb>>::deserialize(deserializer)?;
        let mut colors = [None; COUNT];
        for (index, value) in values.into_iter().take(COUNT).enumerate() {
            colors[index] = value;
        }
        Ok(Self(colors))
    }
}

impl Default for Colors {
    fn default() -> Self {
        Self([None; COUNT])
    }
}

impl Index<usize> for Colors {
    type Output = Option<Rgb>;

    #[inline]
    fn index(&self, index: usize) -> &Self::Output {
        &self.0[index]
    }
}

impl IndexMut<usize> for Colors {
    #[inline]
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        &mut self.0[index]
    }
}

impl Index<NamedColor> for Colors {
    type Output = Option<Rgb>;

    #[inline]
    fn index(&self, index: NamedColor) -> &Self::Output {
        &self.0[index as usize]
    }
}

impl IndexMut<NamedColor> for Colors {
    #[inline]
    fn index_mut(&mut self, index: NamedColor) -> &mut Self::Output {
        &mut self.0[index as usize]
    }
}
