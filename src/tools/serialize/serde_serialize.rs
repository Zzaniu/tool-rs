use chrono::NaiveDateTime;
use serde::{self, de::DeserializeOwned, ser::Serialize, Deserialize};
use std::io::{BufRead, BufReader, Write};

pub fn dumps_vec<T>(data: &Vec<T>, path: &std::path::Path)
where
    T: Serialize,
{
    let file = std::fs::File::create(path)
        .unwrap_or_else(|_| panic!("创建文件`{}`失败", path.to_str().unwrap()));
    for item in data {
        let serde_value = serde_json::to_string(&item).expect("序列化失败");
        writeln!(&file, "{}", serde_value).unwrap();
    }
}

pub fn loads_vec<T>(path: &std::path::Path, capacity: usize) -> Vec<T>
where
    T: DeserializeOwned,
{
    let file = std::fs::File::open(path)
        .unwrap_or_else(|_| panic!("打开文件`{}`失败", path.to_str().unwrap()));
    let reader = BufReader::new(file);
    let mut ret: Vec<T> = Vec::with_capacity(capacity);
    for line in reader.lines() {
        let line = line.unwrap();
        let item: T = serde_json::from_str(&line).unwrap();
        ret.push(item)
    }
    ret
}

pub fn dumps<T>(data: &T, path: &std::path::Path)
where
    T: Serialize,
{
    serde_json::to_writer(std::fs::File::create(path).unwrap(), data).unwrap();
}

pub fn loads<T>(path: &std::path::Path) -> T
where
    T: DeserializeOwned,
{
    serde_json::from_reader(std::fs::File::open(path).unwrap()).unwrap()
}

pub fn option_decimal_is_zero(num: &Option<rust_decimal::Decimal>) -> bool {
    num.is_none() || rust_decimal::Decimal::ZERO.cmp(&num.unwrap()).is_eq()
}

pub fn decimal_is_zero(num: &rust_decimal::Decimal) -> bool {
    rust_decimal::Decimal::ZERO.cmp(num).is_eq()
}

pub fn option_naive_datetime_to_date_string<S>(
    value: &Option<NaiveDateTime>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(
        value
            .map(|v| v.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_default()
            .as_str(),
    )
}

pub fn naive_datetime_from_str<'de, D>(deserializer: D) -> Result<NaiveDateTime, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    if s.len() == 10 {
        Ok(
            NaiveDateTime::parse_from_str(&format!("{} 00:00:00", s), "%Y-%m-%d %H:%M:%S")
                .map_err(serde::de::Error::custom)?,
        )
    } else {
        Ok(NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S")
            .map_err(serde::de::Error::custom)?)
    }
}

pub fn option_naive_datetime_from_str<'de, D>(
    deserializer: D,
) -> Result<Option<NaiveDateTime>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct OptionNaiveDateTimeVisitor;

    impl<'de> serde::de::Visitor<'de> for OptionNaiveDateTimeVisitor {
        type Value = Option<NaiveDateTime>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("an Option<String>")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            if value.is_empty() {
                Ok(None)
            } else if value.len() == 10 {
                Ok(Some(
                    NaiveDateTime::parse_from_str(
                        &format!("{} 00:00:00", value),
                        "%Y-%m-%d %H:%M:%S",
                    )
                    .map_err(serde::de::Error::custom)?,
                ))
            } else {
                Ok(Some(
                    NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
                        .map_err(serde::de::Error::custom)?,
                ))
            }
        }

        // 如果没有传值, 则会走到这里面来
        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: serde::de::Deserializer<'de>,
        {
            self.visit_string(String::deserialize(deserializer)?)
        }

        // 如果是 null 值, 则会走到这里面来
        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            println!("visit_unit");
            Ok(None)
        }
    }

    deserializer.deserialize_option(OptionNaiveDateTimeVisitor)
}

pub fn naive_datetime_to_date_string<S>(
    value: &NaiveDateTime,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(value.format("%Y-%m-%d %H:%M:%S").to_string().as_str())
}

#[macro_export]
macro_rules! impl_numeric_type_for_integers {
    ($($integer_type:ty),*) => {
        $(
            // TODO: 这里的 ::paste::paste! 不知道是干嘛用的, 需要研究一下
            ::paste::paste! {
                pub fn [<$integer_type _is_zero>](num: &$integer_type) -> bool {
                    *num == 0
                }
            }
        )*
    };
}

impl_numeric_type_for_integers!(i8, i16, i32, i64, i128, isize, u8, u16, u32, u64, u128, usize);
