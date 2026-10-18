//! UTRAN.CSV (インタンクの給油機が USB に書く SJIS の CSV) の decode と行の解釈 (純粋ロジック)。
//! CakePHP の FuelIntankController::uploadImport (fgetcsv + mb_convert_encoding) と
//! FuelIntankTable::beforeMarshal・is_start・is_fuel・setDatetime を写している。
//!
//! - 1 列目が `Z` の行は start (給油機の始業): 日時 = 3 列目 (yymmdd) + 4 列目 (hhmm)・detail = `5 列目-6 列目`・
//!   fuel_card_id = 9999・quantity = NULL
//! - 1 列目が `1` の行は fuel (給油): 日時 = 21 列目 (yymmdd) + 1 列目 (hhmm)・fuel_card_id = 7 列目・
//!   quantity = 13 列目 (0.01 L)・slip_num = 3 列目
//! - それ以外の行は無視する (列番号は 0 始まり)
//!
//! 数字が読めない・日時が暦に無い・detail が `A-Za-z0-9_-` の 32 文字以内でない行はエラー (行番号と理由だけ返す)。
//! slip_num (start 行)・slip_num_parent は [`crate::slip`] が埋める。

use chrono::{NaiveDate, NaiveDateTime};
use kintai_mysql::bind::Token;

use crate::slip;

/// start 行の fuel_card_id (CakePHP と同じ固定値)。
pub const START_CARD_ID: u64 = 9999;

/// 行の種類 (`fuel_intank.type`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Start,
    Fuel,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Start => "start",
            Kind::Fuel => "fuel",
        }
    }
}

/// 取り込む 1 行 (`fuel_intank` の 1 行になる)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// CSV の行番号 (1 始まり。エラーの報告用)
    pub line: usize,
    pub kind: Kind,
    pub datetime: NaiveDateTime,
    pub fuel_card_id: u64,
    /// 0.01 L 単位。start 行は `None`
    pub quantity: Option<u64>,
    /// fuel 行は CSV の値。start 行は [`slip::set_slip_num`] が前後の行から埋める
    pub slip_num: Option<u64>,
    /// fuel 行の直前の start 行の slip_num ([`slip::set_slip_num_parent`] が埋める)
    pub slip_num_parent: Option<u64>,
    /// start 行の `EP900-34` の形 (fuel 行は `None`)
    pub detail: Option<Token>,
}

/// 解釈できなかった行 (行番号と理由だけ。中身は持たない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub reason: &'static str,
}

/// SJIS (CP932) の bytes を UTF-8 の文字列にする。読めないバイトは U+FFFD になる (使う列に入っていれば行のエラーになる)。
pub fn decode(bytes: &[u8]) -> String {
    encoding_rs::SHIFT_JIS.decode(bytes).0.into_owned()
}

/// CSV の bytes を読んで、slip_num・slip_num_parent まで埋めた行の列を返す。
pub fn read(bytes: &[u8]) -> Result<Vec<Row>, ParseError> {
    let mut rows = parse(&decode(bytes))?;
    slip::set_slip_num(&mut rows)?;
    slip::set_slip_num_parent(&mut rows);
    Ok(rows)
}

/// 行を解釈する (slip_num の補完はしない)。start でも fuel でもない行は飛ばす。
pub fn parse(text: &str) -> Result<Vec<Row>, ParseError> {
    let mut rows = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let cols: Vec<&str> = raw.split(',').map(unquote).collect();
        let parsed = match cols[0] {
            "Z" => start_row(&cols),
            "1" => fuel_row(&cols),
            _ => continue,
        };
        let line = i + 1;
        let mut row = parsed.map_err(|reason| ParseError { line, reason })?;
        row.line = line;
        rows.push(row);
    }
    Ok(rows)
}

/// 前後の `"` を外す (fgetcsv が外す囲み。中の `""` は扱わない)。
fn unquote(s: &str) -> &str {
    let s = s.trim_end_matches('\r');
    match s.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
        Some(inner) => inner,
        None => s,
    }
}

fn col<'a>(cols: &[&'a str], i: usize) -> Result<&'a str, &'static str> {
    cols.get(i).copied().ok_or("missing_column")
}

/// 数字だけの列を整数にする (`00042001` → 42001)。空・数字以外・桁あふれはエラー。
fn uint(cols: &[&str], i: usize) -> Result<u64, &'static str> {
    let s = col(cols, i)?;
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err("not_a_number");
    }
    s.parse().map_err(|_| "not_a_number")
}

/// `yymmdd` + `hhmm` (時刻は 4 桁に左を 0 で埋める。`733` → `07:33`)。年は 20yy。
fn datetime(cols: &[&str], date_i: usize, time_i: usize) -> Result<NaiveDateTime, &'static str> {
    let (date, time) = (col(cols, date_i)?, col(cols, time_i)?);
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if date.len() != 6 || !digits(date) || time.is_empty() || time.len() > 4 || !digits(time) {
        return Err("bad_datetime");
    }
    let time = format!("{time:0>4}");
    let num = |s: &str| s.parse::<u32>().unwrap_or_default();
    let (y, m, d) = (num(&date[0..2]), num(&date[2..4]), num(&date[4..6]));
    let (h, min) = (num(&time[0..2]), num(&time[2..4]));
    NaiveDate::from_ymd_opt(2000 + y as i32, m, d)
        .and_then(|day| day.and_hms_opt(h, min, 0))
        .ok_or("bad_datetime")
}

fn start_row(cols: &[&str]) -> Result<Row, &'static str> {
    let datetime = datetime(cols, 3, 4)?;
    let detail = format!("{}-{}", col(cols, 5)?, col(cols, 6)?);
    let detail = Token::new(&detail).ok_or("bad_detail")?;
    Ok(Row {
        line: 0,
        kind: Kind::Start,
        datetime,
        fuel_card_id: START_CARD_ID,
        quantity: None,
        slip_num: None,
        slip_num_parent: None,
        detail: Some(detail),
    })
}

fn fuel_row(cols: &[&str]) -> Result<Row, &'static str> {
    Ok(Row {
        line: 0,
        kind: Kind::Fuel,
        datetime: datetime(cols, 21, 1)?,
        fuel_card_id: uint(cols, 7)?,
        quantity: Some(uint(cols, 13)?),
        slip_num: Some(uint(cols, 3)?),
        slip_num_parent: None,
        detail: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    const Z: &str = "Z,71043,43007,231220,0733,EP900,34,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0";
    const F: &str =
        "1,0827,6698,5523,3,43007,00019,0057,0,0,001,0003,0,00042001,0,0,0,A,0,0,0,231220";

    #[test]
    fn start_row_fields() {
        let rows = parse(Z).unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(
            (r.line, r.kind, r.datetime, r.fuel_card_id, r.quantity),
            (1, Kind::Start, dt("2023-12-20 07:33"), 9999, None)
        );
        assert_eq!(r.detail.unwrap().as_str(), "EP900-34");
        assert_eq!((r.slip_num, r.slip_num_parent), (None, None));
        assert_eq!(Kind::Start.as_str(), "start");
    }

    #[test]
    fn fuel_row_fields() {
        let r = &parse(F).unwrap()[0];
        assert_eq!(
            (r.kind, r.datetime, r.fuel_card_id, r.quantity, r.slip_num),
            (
                Kind::Fuel,
                dt("2023-12-20 08:27"),
                57,
                Some(42001),
                Some(5523)
            )
        );
        assert_eq!((r.detail, r.slip_num_parent), (None, None));
        assert_eq!(Kind::Fuel.as_str(), "fuel");
    }

    #[test]
    fn short_time_is_zero_padded() {
        let z = Z.replace(",0733,", ",733,");
        assert_eq!(parse(&z).unwrap()[0].datetime, dt("2023-12-20 07:33"));
        let z = Z.replace(",0733,", ",5,");
        assert_eq!(parse(&z).unwrap()[0].datetime, dt("2023-12-20 00:05"));
    }

    #[test]
    fn other_rows_are_ignored_and_line_numbers_kept() {
        let text = format!("H,header\n\n{Z}\r\n2,x\n{F}\r\n");
        let rows = parse(&text).unwrap();
        assert_eq!(rows.iter().map(|r| r.line).collect::<Vec<_>>(), [3, 5]);
    }

    #[test]
    fn quoted_fields_are_unquoted() {
        let z = Z.replace("EP900", "\"EP900\"").replacen('Z', "\"Z\"", 1);
        assert_eq!(parse(&z).unwrap()[0].detail.unwrap().as_str(), "EP900-34");
    }

    #[test]
    fn bad_rows_report_line_and_reason() {
        let cases = [
            (F.replace(",0057,", ",57a,"), "not_a_number"),
            (F.replace(",00042001,", ",,"), "not_a_number"),
            (
                F.replace(",0057,", ",99999999999999999999,"),
                "not_a_number",
            ),
            (F.replace(",231220", ",231340"), "bad_datetime"),
            (F.replace(",231220", ",2312"), "bad_datetime"),
            (F.replace(",0827,", ",2460,"), "bad_datetime"),
            (F.replace(",0827,", ",08270,"), "bad_datetime"),
            (F.replace(",0827,", ",,"), "bad_datetime"),
            (F.replace(",0827,", ",8a,"), "bad_datetime"),
            (F.replace(",231220", ",23122x"), "bad_datetime"),
            (F.replace(",231220", ""), "missing_column"),
            ("Z,1,2,231220".to_string(), "missing_column"),
            (Z.replace("EP900", "EP 900"), "bad_detail"),
            (Z.replace(",0733,", ",07:3,"), "bad_datetime"),
        ];
        for (line, reason) in cases {
            let text = format!("H\n{line}");
            assert_eq!(parse(&text), Err(ParseError { line: 2, reason }), "{line}");
        }
    }

    #[test]
    fn decode_reads_sjis() {
        // 「給油」の SJIS
        let bytes = [0x8b, 0x8b, 0x96, 0xfb, b',', b'1'];
        assert_eq!(decode(&bytes), "給油,1");
    }

    #[test]
    fn read_fills_slip_numbers() {
        let rows = read(format!("{Z}\n{F}\n").as_bytes()).unwrap();
        assert_eq!(rows[0].slip_num, Some(5522));
        assert_eq!(rows[1].slip_num_parent, Some(5522));
        let lone = read(Z.as_bytes());
        assert_eq!(
            lone,
            Err(ParseError {
                line: 1,
                reason: "slip_num_undecidable"
            })
        );
        assert_eq!(read(b"bad"), Ok(vec![]));
        assert!(read(F.replace(",0057,", ",x,").as_bytes()).is_err());
    }
}
