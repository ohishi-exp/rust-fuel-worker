//! UTRAN20231221.CSV (CakePHP の tests/files/ の写し。SJIS) の golden test。
//! 期待値は PHP の規則 (beforeMarshal・setSlipNum・setSlipNumParent) から手で求めて固定している
//! (CakePHP の FuelIntankControllerTest::testUploadImport の 26 行・5522・5547 とも一致する)。

use chrono::NaiveDateTime;
use fuel_logic::csv::{read, Kind, Row};
use fuel_logic::ingest::{existing_sql, insert_sql, plan};

const CSV: &[u8] = include_bytes!("fixtures/UTRAN20231221.CSV");

fn dt(s: &str) -> NaiveDateTime {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
}

type Fields<'a> = (
    Kind,
    NaiveDateTime,
    u64,
    Option<u64>,
    Option<u64>,
    Option<u64>,
    Option<&'a str>,
);

fn fields(r: &Row) -> Fields<'_> {
    (
        r.kind,
        r.datetime,
        r.fuel_card_id,
        r.quantity,
        r.slip_num,
        r.slip_num_parent,
        r.detail.as_ref().map(|d| d.as_str()),
    )
}

#[test]
fn golden_rows() {
    let rows = read(CSV).unwrap();
    assert_eq!(rows.len(), 26);
    let starts = rows.iter().filter(|r| r.kind == Kind::Start).count();
    assert_eq!((starts, rows.len() - starts), (1, 25));
    assert_eq!(rows[0].kind, Kind::Start);
    assert!(rows[1..].iter().all(|r| r.kind == Kind::Fuel));

    // 1 行目: Z,71043,43007,231220,0733,EP900,34,… → slip_num は次の行 (5523) - 1
    let first = (
        Kind::Start,
        dt("2023-12-20 07:33"),
        9999,
        None,
        Some(5522),
        None,
        Some("EP900-34"),
    );
    assert_eq!(fields(&rows[0]), first);
    // 2 行目: 1,0827,6698,5523,…,0057,…,00042001,…,231220
    let second = (
        Kind::Fuel,
        dt("2023-12-20 08:27"),
        57,
        Some(42001),
        Some(5523),
        Some(5522),
        None,
    );
    assert_eq!(fields(&rows[1]), second);
    // 最後の行: 1,2319,6722,5547,…,0031,…,00010003,…,231220
    let last = (
        Kind::Fuel,
        dt("2023-12-20 23:19"),
        31,
        Some(10003),
        Some(5547),
        Some(5522),
        None,
    );
    assert_eq!(fields(&rows[25]), last);

    // fuel 行の slip_num は 5523..=5547 の連番、親は全部 5522
    let slips: Vec<_> = rows[1..].iter().map(|r| r.slip_num.unwrap()).collect();
    assert_eq!(slips, (5523..=5547).collect::<Vec<_>>());
    assert!(rows[1..].iter().all(|r| r.slip_num_parent == Some(5522)));
    let lines: Vec<_> = rows.iter().map(|r| r.line).collect();
    assert_eq!(lines, (1..=26).collect::<Vec<_>>());
}

#[test]
fn golden_sql_is_one_statement_each() {
    let rows = read(CSV).unwrap();
    let select = existing_sql(&rows).unwrap().unwrap();
    assert_eq!(select.matches("SELECT").count(), 1);
    assert_eq!(select.matches("('2023-12-20 ").count(), 26);
    let p = plan(&rows, &Default::default());
    assert_eq!((p.fresh.len(), p.skipped), (26, 0));
    let with_ids: Vec<_> = p.fresh.iter().map(|r| (*r, None)).collect();
    let insert = insert_sql(&with_ids).unwrap().unwrap();
    assert_eq!(insert.matches("INSERT").count(), 1);
    assert!(insert
        .ends_with("('2023-12-20 23:19:00', 31, 5547, 5522, 10003, 'fuel', NULL, NULL, NULL)"));
}
