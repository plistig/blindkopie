use std::borrow::Borrow;
use std::cmp::Ordering;
use std::convert::{TryFrom, TryInto};
use std::hash::{Hash, Hasher};

use anyhow::Result;
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use serde::{de, ser};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Illegal UNIX timestamp: {0}")]
    CannotConvertToTimestamp(i64),
}

#[derive(Debug, Clone)]
pub struct Timestamp {
    unix_ts: i64,
    utc: DateTime<Utc>,
}

impl Borrow<DateTime<Utc>> for Timestamp {
    fn borrow(&self) -> &DateTime<Utc> {
        &self.utc
    }
}

impl Default for Timestamp {
    fn default() -> Self {
        Utc.ymd(2020, 1, 1).and_hms(12, 0, 0).try_into().unwrap()
    }
}

impl Ord for Timestamp {
    fn cmp(&self, other: &Self) -> Ordering {
        self.unix_ts.cmp(&other.unix_ts)
    }
}

impl PartialOrd for Timestamp {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.unix_ts.partial_cmp(&other.unix_ts)
    }
}

impl Eq for Timestamp {}

impl PartialEq for Timestamp {
    fn eq(&self, other: &Self) -> bool {
        self.unix_ts.eq(&other.unix_ts)
    }
}

impl Hash for Timestamp {
    fn hash<H>(&self, hasher: &mut H)
    where
        H: Hasher,
    {
        self.unix_ts.hash(hasher)
    }
}

impl TryFrom<f64> for Timestamp {
    type Error = Error;

    fn try_from(created_utc: f64) -> Result<Self, Self::Error> {
        (created_utc as i64).try_into()
    }
}

impl TryFrom<i64> for Timestamp {
    type Error = Error;

    fn try_from(unix_ts: i64) -> Result<Self, Self::Error> {
        let utc = NaiveDateTime::from_timestamp_opt(unix_ts, 0)
            .ok_or(Error::CannotConvertToTimestamp(unix_ts))?;
        let utc = DateTime::from_utc(utc, Utc);
        Ok(Self { unix_ts, utc })
    }
}

impl From<DateTime<Utc>> for Timestamp {
    fn from(utc: DateTime<Utc>) -> Self {
        let unix_ts = utc.timestamp();
        Self { unix_ts, utc }
    }
}

struct TimestampVisitor;

impl<'de> de::Visitor<'de> for TimestampVisitor {
    type Value = Timestamp;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        formatter.write_str("a timestamp")
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Timestamp::try_from(value).map_err(E::custom)
    }
}

impl<'de> de::Deserialize<'de> for Timestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        deserializer.deserialize_i64(TimestampVisitor)
    }
}

impl ser::Serialize for Timestamp {
    fn serialize<S>(
        &self,
        serializer: S,
    ) -> Result<<S as ser::Serializer>::Ok, <S as ser::Serializer>::Error>
    where
        S: ser::Serializer,
    {
        serializer.serialize_i64(self.unix_ts)
    }
}
