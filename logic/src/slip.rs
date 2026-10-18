//! 伝票番号の補完 (純粋ロジック)。CakePHP の FuelIntankTable::setSlipNum・setSlipNumParent を写している。
//!
//! - setSlipNum: slip_num の無い行 (start 行) は、次の行があれば「次の行の slip_num - 1」、無ければ「前の行 + 1」。
//!   前から順に埋める (前の行は埋め終わっている)
//! - setSlipNumParent: fuel 行の slip_num_parent は、直前の start 行の slip_num (start 行より前の fuel 行は NULL)
//!
//! PHP と違うところ: 次の行も slip_num を持たない (start 行が 2 つ続く)・slip_num が 0 で 1 を引けない・
//! 前後の行が無い (start 行だけの CSV) ときは、PHP は -1 や NULL を入れる (後者は検証で保存が落ちる) が、
//! ここでは行のエラーにする。

use crate::csv::{Kind, ParseError, Row};

const UNDECIDABLE: &str = "slip_num_undecidable";

/// start 行の slip_num を前後の行から埋める。決められない行があればエラー。
pub fn set_slip_num(rows: &mut [Row]) -> Result<(), ParseError> {
    for i in 0..rows.len() {
        if rows[i].slip_num.is_some() {
            continue;
        }
        let num = match (rows.get(i + 1), i.checked_sub(1).map(|p| &rows[p])) {
            (Some(next), _) => next.slip_num.and_then(|n| n.checked_sub(1)),
            (None, Some(prev)) => prev.slip_num.map(|n| n + 1),
            (None, None) => None,
        };
        let line = rows[i].line;
        rows[i].slip_num = Some(num.ok_or(ParseError {
            line,
            reason: UNDECIDABLE,
        })?);
    }
    Ok(())
}

/// fuel 行の slip_num_parent に直前の start 行の slip_num を入れる。
pub fn set_slip_num_parent(rows: &mut [Row]) {
    let mut parent = None;
    for row in rows.iter_mut() {
        match row.kind {
            Kind::Start => parent = row.slip_num,
            Kind::Fuel => row.slip_num_parent = parent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDateTime;

    fn row(line: usize, kind: Kind, slip: Option<u64>) -> Row {
        Row {
            line,
            kind,
            datetime: NaiveDateTime::default(),
            fuel_card_id: 1,
            quantity: None,
            slip_num: slip,
            slip_num_parent: None,
            detail: None,
        }
    }

    fn slips(rows: &[Row]) -> Vec<Option<u64>> {
        rows.iter().map(|r| r.slip_num).collect()
    }

    #[test]
    fn start_takes_next_minus_one() {
        let mut rows = vec![
            row(1, Kind::Start, None),
            row(2, Kind::Fuel, Some(10)),
            row(3, Kind::Start, None),
            row(4, Kind::Fuel, Some(20)),
        ];
        set_slip_num(&mut rows).unwrap();
        assert_eq!(slips(&rows), [Some(9), Some(10), Some(19), Some(20)]);
    }

    #[test]
    fn last_start_takes_prev_plus_one() {
        let mut rows = vec![row(1, Kind::Fuel, Some(10)), row(2, Kind::Start, None)];
        set_slip_num(&mut rows).unwrap();
        assert_eq!(slips(&rows), [Some(10), Some(11)]);
    }

    #[test]
    fn undecidable_is_error() {
        let err = |line| {
            Err(ParseError {
                line,
                reason: UNDECIDABLE,
            })
        };
        // start だけ
        assert_eq!(set_slip_num(&mut [row(7, Kind::Start, None)]), err(7));
        // start が 2 つ続く (次の行も slip_num を持たない)
        let mut two = [
            row(1, Kind::Start, None),
            row(2, Kind::Start, None),
            row(3, Kind::Fuel, Some(5)),
        ];
        assert_eq!(set_slip_num(&mut two), err(1));
        // 次の行が 0 で 1 を引けない
        let mut zero = [row(1, Kind::Start, None), row(2, Kind::Fuel, Some(0))];
        assert_eq!(set_slip_num(&mut zero), err(1));
        assert_eq!(set_slip_num(&mut []), Ok(()));
    }

    #[test]
    fn parent_is_latest_start() {
        let mut rows = vec![
            row(1, Kind::Fuel, Some(1)),
            row(2, Kind::Start, Some(9)),
            row(3, Kind::Fuel, Some(10)),
            row(4, Kind::Fuel, Some(11)),
            row(5, Kind::Start, Some(19)),
            row(6, Kind::Fuel, Some(20)),
        ];
        set_slip_num_parent(&mut rows);
        let parents: Vec<_> = rows.iter().map(|r| r.slip_num_parent).collect();
        assert_eq!(parents, [None, None, Some(9), Some(9), None, Some(19)]);
    }
}
