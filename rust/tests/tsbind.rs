use aimap::pgtime::Ts;
use postgres::{Client, NoTls};
use jiff::Timestamp;

#[test]
fn ts_binds_and_round_trips() {
    let mut conn = Client::connect(&std::env::var("TEST_DATABASE_URL").unwrap(), NoTls).unwrap();
    let t = Timestamp::from_second(1767225600).unwrap();
    let got: i64 = conn
        .query_one("SELECT extract(epoch from $1::timestamptz)::bigint", &[&Ts(t)])
        .unwrap()
        .get(0);
    assert_eq!(got, 1767225600);
    // and through a WHERE clause like the inbox queries
    let n: i64 = conn
        .query_one("SELECT count(*) FROM (SELECT 1) x WHERE now() >= $1", &[&Ts(t)])
        .unwrap()
        .get(0);
    assert_eq!(n, 1);
}
