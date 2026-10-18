//! MariaDB の接続の種別と、資格情報を読む Secrets Store の binding 名の対応。
//!
//! 読み取りは勤怠 Worker と共用の `KINTAI_MARIADB`、書き込みは fuel 専用の `FUEL_MARIADB_WRITE`。
//! 書き込みの口は前段の SELECT (既存キーの確認・dtako の照合) も Write の接続で行う (1 リクエスト 1 接続)。

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

impl Access {
    /// 資格情報の Secrets Store の binding 名 (wrangler.toml の `binding` と同名)。
    pub fn secret_binding(self) -> &'static str {
        match self {
            Access::Read => "KINTAI_MARIADB",
            Access::Write => "FUEL_MARIADB_WRITE",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_per_access() {
        assert_eq!(Access::Read.secret_binding(), "KINTAI_MARIADB");
        assert_eq!(Access::Write.secret_binding(), "FUEL_MARIADB_WRITE");
        assert_ne!(Access::Read, Access::Write);
        assert_eq!(format!("{:?}", Access::Read.clone()), "Read");
    }
}
