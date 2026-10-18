//! 給油の行に dtako_rows の運行 (dtako_row_id) を紐づける判定と SQL 定数 (純粋ロジック)。
//! CakePHP の FuelIntankTable::getDtakoRowID・DtakoRowWithInTime・DtakoRowAfter1hour と
//! FuelCardsTable::findCurrentOne を写している。
//!
//! 1. fuel_cards の現行カード (`end_datetime IS NULL`、car_id の昇順の先頭) → car_id ([`CARD_SQL`])
//! 2. dtako_rows の中で `出庫日時 <= dt <= 帰庫日時`・`車輌CC = car_id`・`対象乗務員区分 = 1` の行 ([`WITHIN_SQL`])
//! 3. 無ければ `帰庫日時 <= dt` の中で `出庫日時` の降順の先頭を取り、`帰庫日時 + 1 時間 > dt` なら採る ([`AFTER_SQL`])
//! 4. それも無い・カードが無い・start 行は NULL (PHP のカード無しの `false` も NULL にする)
//!
//! PHP の DtakoRowWithInTime は並びを指定しない `first()` だが、ここでは `出庫日時` の降順の先頭に固定する
//! (重なる運行が 2 つあるときに結果が決まるように)。

use chrono::{Duration, NaiveDateTime};
use kintai_mysql::bind::{Digits, Token, Value};

use crate::api::parse_datetime;
use crate::csv::{Kind, Row};

/// 現行のカードの car_id。
pub const CARD_SQL: &str = "SELECT car_id FROM fuel_cards \
WHERE id = :fuel_card_id AND end_datetime IS NULL ORDER BY car_id LIMIT 1";

/// 給油の時刻を含む運行。
pub const WITHIN_SQL: &str = "SELECT id, `出庫日時`, `帰庫日時` FROM dtako_rows \
WHERE `出庫日時` <= :datetime AND `帰庫日時` >= :datetime AND `車輌CC` = :car_id AND `対象乗務員区分` = 1 \
ORDER BY `出庫日時` DESC LIMIT 1";

/// 給油の時刻より前に帰庫した運行のうち、出庫が最も遅いもの。
pub const AFTER_SQL: &str = "SELECT id, `出庫日時`, `帰庫日時` FROM dtako_rows \
WHERE `帰庫日時` <= :datetime AND `車輌CC` = :car_id AND `対象乗務員区分` = 1 \
ORDER BY `出庫日時` DESC LIMIT 1";

/// 帰庫の後、この時間より前の給油までは同じ運行に付ける。
const AFTER_RETURN: Duration = Duration::hours(1);

/// dtako_rows の 1 行 (id・出庫日時・帰庫日時)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub id: Digits,
    pub departure: NaiveDateTime,
    pub arrival: NaiveDateTime,
}

/// DB の文字列 (`id` と `YYYY-MM-DD HH:MM:SS` の 2 つ) から [`Span`] を作る。どれかが読めなければ `None`。
pub fn span(id: Option<&str>, departure: Option<&str>, arrival: Option<&str>) -> Option<Span> {
    Some(Span {
        id: Digits::new(id?)?,
        departure: parse_datetime(departure?)?,
        arrival: parse_datetime(arrival?)?,
    })
}

/// 照合が要る行か (fuel 行だけ。start 行は NULL)。
pub fn needs_match(row: &Row) -> bool {
    row.kind == Kind::Fuel
}

/// [`CARD_SQL`] の引数。
pub fn card_params(fuel_card_id: u64) -> [(&'static str, Value); 1] {
    [("fuel_card_id", Value::UInt(fuel_card_id))]
}

/// [`CARD_SQL`] が返した car_id を [`WITHIN_SQL`]・[`AFTER_SQL`] の引数にする。
/// NULL・`A-Za-z0-9_-` の 32 文字以内でない car_id は照合しない (`None`)。
pub fn span_params(
    car_id: Option<&str>,
    datetime: NaiveDateTime,
) -> Option<[(&'static str, Value); 2]> {
    let car = Token::new(car_id?)?;
    Some([
        ("datetime", Value::DateTime(datetime)),
        ("car_id", Value::Token(car)),
    ])
}

/// 2 つの問い合わせの結果から dtako_row_id を決める。
/// `within` は [`WITHIN_SQL`] の結果、`after` は [`AFTER_SQL`] の結果 (within で決まれば読まなくてよい)。
pub fn decide(
    datetime: NaiveDateTime,
    within: Option<Span>,
    after: Option<Span>,
) -> Option<Digits> {
    if let Some(s) = within.filter(|s| s.departure <= datetime && datetime <= s.arrival) {
        return Some(s.id);
    }
    after
        .filter(|s| s.arrival <= datetime && s.arrival + AFTER_RETURN > datetime)
        .map(|s| s.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kintai_mysql::bind::{expand, names};

    fn dt(s: &str) -> NaiveDateTime {
        parse_datetime(s).unwrap()
    }

    fn sp(id: &str, dep: &str, arr: &str) -> Span {
        span(Some(id), Some(dep), Some(arr)).unwrap()
    }

    const DEP: &str = "2023-12-20 06:00:00";
    const ARR: &str = "2023-12-20 18:00:00";

    #[test]
    fn within_includes_both_ends() {
        let s = sp("23122006000000000571", DEP, ARR);
        for t in [DEP, "2023-12-20 12:00:00", ARR] {
            assert_eq!(decide(dt(t), Some(s), None), Some(s.id), "{t}");
        }
        assert_eq!(decide(dt("2023-12-20 05:59:59"), Some(s), None), None);
    }

    #[test]
    fn after_return_within_one_hour() {
        let s = sp("1", DEP, ARR);
        assert_eq!(decide(dt(ARR), None, Some(s)), Some(s.id));
        assert_eq!(decide(dt("2023-12-20 18:59:00"), None, Some(s)), Some(s.id));
        assert_eq!(decide(dt("2023-12-20 18:59:59"), None, Some(s)), Some(s.id));
        assert_eq!(decide(dt("2023-12-20 19:00:00"), None, Some(s)), None);
        assert_eq!(decide(dt("2023-12-20 17:00:00"), None, Some(s)), None);
    }

    #[test]
    fn within_wins_and_falls_back_to_after() {
        let (w, a) = (sp("2", DEP, ARR), sp("3", DEP, "2023-12-20 11:30:00"));
        assert_eq!(
            decide(dt("2023-12-20 12:00:00"), Some(w), Some(a)),
            Some(w.id)
        );
        let out = sp("4", "2023-12-21 06:00:00", "2023-12-21 07:00:00");
        assert_eq!(
            decide(dt("2023-12-20 12:00:00"), Some(out), Some(a)),
            Some(a.id)
        );
        assert_eq!(decide(dt("2023-12-20 12:00:00"), None, None), None);
    }

    #[test]
    fn span_needs_all_three() {
        assert!(span(None, Some(DEP), Some(ARR)).is_none());
        assert!(span(Some("12a"), Some(DEP), Some(ARR)).is_none());
        assert!(span(Some("1"), None, Some(ARR)).is_none());
        assert!(span(Some("1"), Some("x"), Some(ARR)).is_none());
        assert!(span(Some("1"), Some(DEP), None).is_none());
    }

    #[test]
    fn needs_match_only_fuel() {
        let mut row = crate::csv::parse("1,0827,1,2,3,4,5,0057,0,0,0,0,0,1,0,0,0,A,0,0,0,231220")
            .unwrap()
            .remove(0);
        assert!(needs_match(&row));
        row.kind = Kind::Start;
        assert!(!needs_match(&row));
    }

    #[test]
    fn sql_names_match_params() {
        let p = span_params(Some("535500"), dt(DEP)).unwrap();
        for sql in [WITHIN_SQL, AFTER_SQL] {
            assert_eq!(names(sql).unwrap(), ["datetime", "car_id"]);
            let out = expand(sql, &p).unwrap();
            assert!(out.contains("<= '2023-12-20 06:00:00'") && out.contains("= '535500'"));
        }
        let out = expand(CARD_SQL, &card_params(57)).unwrap();
        assert!(out.contains("id = 57 AND"));
        assert!(span_params(None, dt(DEP)).is_none());
        assert!(span_params(Some("53 55"), dt(DEP)).is_none());
    }
}
