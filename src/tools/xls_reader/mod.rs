use std::sync::OnceLock;

pub use calamine::{self, CellType, Range};
pub use regex::{self, Regex};

#[derive(Debug, Default)]
pub struct CellCoordinates {
    pub row: u32,
    pub col: u32,
}

impl CellCoordinates {
    pub fn new(row: u32, col: u32) -> Self {
        Self { row, col }
    }
}

impl From<String> for CellCoordinates {
    fn from(value: String) -> Self {
        let str_ref: &str = value.as_ref();
        str_ref.into()
    }
}

impl From<&str> for CellCoordinates {
    fn from(value: &str) -> Self {
        let coordinate_upper = value.to_uppercase();
        index_from_coordinate(coordinate_upper)
    }
}

impl From<CellCoordinates> for (u32, u32) {
    fn from(value: CellCoordinates) -> Self {
        (value.row, value.col)
    }
}

pub struct SheetRange<'a, T>(pub &'a Range<T>);

impl<'a, T> SheetRange<'a, T>
where
    T: CellType,
{
    pub fn new(range: &'a Range<T>) -> Self {
        Self(range)
    }

    pub fn get_value<V: Into<CellCoordinates>>(&self, coordinate: V) -> Option<&T> {
        let coordinate: CellCoordinates = coordinate.into();
        self.0.get_value(coordinate.into())
    }
}

fn alpha_to_index<S>(alpha: S) -> u32
where
    S: AsRef<str>,
{
    const BASE_CHAR_CODE: u32 = 'A' as u32;
    // since we only allow up to three characters, we can use pre-computed
    /// powers of 26 `[26^0, 26^1, 26^2]`
    const POSITIONAL_CONSTANTS: [u32; 3] = [1, 26, 676];

    alpha
        .as_ref()
        .chars()
        .rev()
        .enumerate()
        .map(|(index, v)| {
            let vn = (v as u32 - BASE_CHAR_CODE) + 1;

            // 26u32.pow(index as u32) * vn
            POSITIONAL_CONSTANTS[index] * vn
        })
        .sum::<u32>()
        - 1
}

#[allow(unused)]
pub fn column_index_from_string<S: AsRef<str>>(column: S) -> u32 {
    let column_c = column.as_ref();
    if column_c == "0" {
        return 0;
    }

    alpha_to_index(column_c)
}

pub fn index_from_coordinate<T>(coordinate: T) -> CellCoordinates
where
    T: AsRef<str>,
{
    static RE: OnceLock<Regex> = OnceLock::new();

    let caps = RE
        .get_or_init(|| Regex::new(r"((\$)?([A-Z]{1,3}))?((\$)?([0-9]+))?").unwrap())
        .captures(coordinate.as_ref());

    caps.map(|v| {
        let col = v.get(3).map(|v| alpha_to_index(v.as_str())); // col number: [A-Z]{1,3}
        let row = v
            .get(6)
            .and_then(|v| v.as_str().parse::<u32>().ok().map(|v| v - 1)); // row number: [0-9]
        CellCoordinates::new(row.unwrap(), col.unwrap())
    })
    .unwrap_or_default()
}

#[test]
fn text_to_index_test() {
    assert_eq!(0, column_index_from_string("A"));
    assert_eq!(25, column_index_from_string("Z"));
    assert_eq!(26, column_index_from_string("AA"));
    assert_eq!(51, column_index_from_string("AZ"));
    assert_eq!(52, column_index_from_string("BA"));
    assert_eq!(77, column_index_from_string("BZ"));
    assert_eq!(78, column_index_from_string("CA"));
    assert_eq!(675, column_index_from_string("YZ"));
    assert_eq!(676, column_index_from_string("ZA"));
    assert_eq!(701, column_index_from_string("ZZ"));
    assert_eq!(702, column_index_from_string("AAA"));
}
