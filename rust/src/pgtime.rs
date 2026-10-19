//! `jiff::Timestamp` <-> Postgres TIMESTAMPTZ binding (microseconds since the
//! PG epoch, 2000-01-01), so catalog/inbox rows can move timestamps directly.

use jiff::Timestamp;
use postgres::types::{FromSql, ToSql, Type};
use std::error::Error;

/// 2000-01-01T00:00:00Z, in unix microseconds
const PG_EPOCH_US: i64 = 946_684_800_000_000;

#[derive(Debug)]
pub struct Ts(pub Timestamp);

impl ToSql for Ts {
    fn to_sql_checked(
        &self,
        ty: &Type,
        out: &mut postgres::types::private::BytesMut,
    ) -> Result<postgres::types::IsNull, Box<dyn Error + Sync + Send>> {
        // the crate binds through this entry point; delegate to the encoder
        self.to_sql(ty, out)
    }

    fn to_sql(
        &self,
        _: &Type,
        out: &mut postgres::types::private::BytesMut,
    ) -> Result<postgres::types::IsNull, Box<dyn Error + Sync + Send>> {
        let us = self.0.as_microsecond() - PG_EPOCH_US;
        out.extend_from_slice(&us.to_be_bytes());
        Ok(postgres::types::IsNull::No)
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::TIMESTAMPTZ || *ty == Type::TIMESTAMP
    }
}

impl FromSql<'_> for Ts {
    fn from_sql(
        _: &Type,
        raw: &[u8],
    ) -> Result<Self, Box<dyn Error + Sync + Send>> {
        let us = i64::from_be_bytes(raw.try_into()?) + PG_EPOCH_US;
        Ok(Ts(Timestamp::from_microsecond(us)?))
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::TIMESTAMPTZ || *ty == Type::TIMESTAMP
    }
}

/// RFC 3339 text for JSON output (FastAPI serialises datetimes the same way).
pub fn iso(ts: Timestamp) -> String {
    ts.to_string()
}

pub fn opt_iso(ts: Option<Timestamp>) -> Option<String> {
    ts.map(iso)
}
