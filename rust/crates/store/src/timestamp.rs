use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// Imported timestamps are opaque text. Native clocks continue to use epoch milliseconds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConversationTimestamp {
    EpochMillis(i64),
    Text(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("timestamp is outside JavaScript Date range")]
pub struct TimestampOutOfRange;

impl From<TimestampOutOfRange> for crate::StoreError {
    fn from(error: TimestampOutOfRange) -> Self {
        Self::InvalidInput(error.to_string())
    }
}

impl ConversationTimestamp {
    pub fn from_epoch_millis(milliseconds: i64) -> Result<Self, TimestampOutOfRange> {
        validate_epoch(milliseconds)?;
        Ok(Self::EpochMillis(milliseconds))
    }

    pub fn wire_text(&self) -> Result<Cow<'_, str>, TimestampOutOfRange> {
        match self {
            Self::EpochMillis(milliseconds) => {
                validate_epoch(*milliseconds)?;
                Ok(Cow::Owned(epoch_text(*milliseconds)))
            }
            Self::Text(text) => Ok(Cow::Borrowed(text)),
        }
    }

    pub fn epoch_millis(&self) -> Option<i64> {
        match self {
            Self::EpochMillis(milliseconds) => Some(*milliseconds),
            Self::Text(_) => None,
        }
    }
}

fn validate_epoch(milliseconds: i64) -> Result<(), TimestampOutOfRange> {
    if (-8_640_000_000_000_000..=8_640_000_000_000_000).contains(&milliseconds) {
        Ok(())
    } else {
        Err(TimestampOutOfRange)
    }
}

impl From<i64> for ConversationTimestamp {
    fn from(milliseconds: i64) -> Self {
        Self::EpochMillis(milliseconds)
    }
}

impl From<&str> for ConversationTimestamp {
    fn from(text: &str) -> Self {
        Self::Text(text.into())
    }
}

impl From<String> for ConversationTimestamp {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

fn epoch_text(milliseconds: i64) -> String {
    let (year, month, day) = civil_date(milliseconds.div_euclid(86_400_000));
    let remainder = milliseconds.rem_euclid(86_400_000);
    let year = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else {
        format!(
            "{}{year:06}",
            if year < 0 { "-" } else { "+" },
            year = year.abs()
        )
    };
    format!(
        "{year}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        remainder / 3_600_000,
        remainder / 60_000 % 60,
        remainder / 1000 % 60,
        remainder % 1000
    )
}

fn civil_date(days: i64) -> (i64, i64, i64) {
    // Integer Gregorian conversion covers JavaScript Date's entire range,
    // including its extended years beyond Chrono's supported calendar.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let march_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * march_month + 2) / 5 + 1;
    let month = march_month + if march_month < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month, day)
}
