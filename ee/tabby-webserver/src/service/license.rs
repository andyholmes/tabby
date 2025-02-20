use anyhow::anyhow;
use async_trait::async_trait;
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use jsonwebtoken as jwt;
use lazy_static::lazy_static;
use serde::Deserialize;
use tabby_db::DbConn;
use tokio::sync::RwLock;

use crate::schema::{
    license::{LicenseInfo, LicenseService, LicenseStatus, LicenseType},
    Result,
};

lazy_static! {
    static ref LICENSE_DECODING_KEY: jwt::DecodingKey =
        jwt::DecodingKey::from_rsa_pem(include_bytes!("../../keys/license.key.pub")).unwrap();
}

#[derive(Debug, Deserialize)]
#[allow(unused)]
struct LicenseJWTPayload {
    /// Expiration time (as UTC timestamp)
    pub exp: i64,

    /// Issued at (as UTC timestamp)
    pub iat: i64,

    /// Issuer
    pub iss: String,

    /// License grantee email address
    pub sub: String,

    /// License Type
    pub typ: LicenseType,

    /// Number of license (# of seats).
    pub num: usize,
}

fn validate_license(token: &str) -> Result<LicenseJWTPayload, jwt::errors::ErrorKind> {
    let mut validation = jwt::Validation::new(jwt::Algorithm::RS512);
    validation.validate_exp = false;
    validation.set_issuer(&["tabbyml.com"]);
    validation.set_required_spec_claims(&["exp", "iat", "sub", "iss"]);
    let data = jwt::decode::<LicenseJWTPayload>(token, &LICENSE_DECODING_KEY, &validation);
    let data = data.map_err(|err| match err.kind() {
        // Map json error (missing failed, parse error) as missing required claims.
        jwt::errors::ErrorKind::Json(err) => {
            jwt::errors::ErrorKind::MissingRequiredClaim(err.to_string())
        }
        _ => err.into_kind(),
    });
    Ok(data?.claims)
}

fn jwt_timestamp_to_utc(secs: i64) -> Result<DateTime<Utc>> {
    Ok(NaiveDateTime::from_timestamp_opt(secs, 0)
        .ok_or_else(|| anyhow!("Timestamp is corrupt"))?
        .and_utc())
}

struct LicenseServiceImpl {
    db: DbConn,
    seats: RwLock<(DateTime<Utc>, usize)>,
}

impl LicenseServiceImpl {
    async fn read_used_seats(&self, force_refresh: bool) -> Result<usize> {
        let now = Utc::now();
        let (refreshed, mut seats) = {
            let lock = self.seats.read().await;
            *lock
        };
        if force_refresh || now - refreshed > Duration::seconds(15) {
            let mut lock = self.seats.write().await;
            seats = self.db.count_active_users().await?;
            *lock = (now, seats);
        }
        Ok(seats)
    }
}

pub async fn new_license_service(db: DbConn) -> Result<impl LicenseService> {
    let seats = db.count_active_users().await?;
    Ok(LicenseServiceImpl {
        db,
        seats: (Utc::now(), seats).into(),
    })
}

fn license_info_from_raw(raw: LicenseJWTPayload, seats_used: usize) -> Result<LicenseInfo> {
    let issued_at = jwt_timestamp_to_utc(raw.iat)?;
    let expires_at = jwt_timestamp_to_utc(raw.exp)?;

    let status = if expires_at < Utc::now() {
        LicenseStatus::Expired
    } else if seats_used > raw.num {
        LicenseStatus::SeatsExceeded
    } else {
        LicenseStatus::Ok
    };

    let license = LicenseInfo {
        r#type: raw.typ,
        status,
        seats: raw.num as i32,
        seats_used: seats_used as i32,
        issued_at,
        expires_at,
    };
    Ok(license)
}

#[async_trait]
impl LicenseService for LicenseServiceImpl {
    async fn read_license(&self) -> Result<Option<LicenseInfo>> {
        let Some(license) = self.db.read_enterprise_license().await? else {
            return Ok(None);
        };
        let license =
            validate_license(&license).map_err(|e| anyhow!("License is corrupt: {e:?}"))?;
        let seats = self.read_used_seats(false).await?;
        let license = license_info_from_raw(license, seats)?;

        Ok(Some(license))
    }

    async fn update_license(&self, license: String) -> Result<()> {
        let raw = validate_license(&license).map_err(|_e| anyhow!("License is not valid"))?;
        let seats = self.read_used_seats(true).await?;
        match license_info_from_raw(raw, seats)?.status {
            LicenseStatus::Ok => self.db.update_enterprise_license(Some(license)).await?,
            LicenseStatus::Expired => return Err(anyhow!("License is expired").into()),
            LicenseStatus::SeatsExceeded => {
                return Err(anyhow!("License doesn't contain sufficient number of seats").into())
            }
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;

    use super::*;

    const VALID_TOKEN: &str = "eyJhbGciOiJSUzUxMiJ9.eyJpc3MiOiJ0YWJieW1sLmNvbSIsInN1YiI6ImZha2VAdGFiYnltbC5jb20iLCJpYXQiOjE3MDUxOTgxMDIsImV4cCI6MTgwNzM5ODcwMiwidHlwIjoiVEVBTSIsIm51bSI6MX0.r99qAkHGAzjZtS904ko5MMklquMcEJdibVGAZAxrJTf-kKBT-Kc-u-A8o7ZSrLD0eubIxNrLb16UsyAMxJ6xnIJY4h8BTIR9cz_dTezyGywpuAKI13Q2S77tfwcyBF6icFkDsz187MSQGPQuTdVNU8zXkYR5ZkNs8_Uc8SL940xt0KHWLU9DX8KT6eCcVMwAypLyAsSTRJeqE8uRumq1K6dKK7wkE_HQrg9nSmr40A5ZZPzRsUp6hShJyMYSp-D02utbT8bAzVPw6alBgZWrmlVEvdcvfO81DZylUIm-pszKityfT5tmuyMWtUx3AeLXSiQWZOpah3OBnL11IKhNhYWSzUMGuDENHfbP9hlSJvzjq8WeN73nXSjkNEVYetT2er6pnoGrvFUBWcLLdWcl4p324WwqsP5A7ZDbWamo62yPxHUy7Vr4ySRLDfNEQbjP8JVPacpx3-5oY16LlzS4e9RhR0G-aykJitrLd5--gTVGxlxsLbmz33TTDd3nMGuQp2xmpZsw4rTKefEN7hCdvgJhtwRLgL4jxSm2mBgtwWH_i0uuBFpCYNgh97rU-Cak66adXDydAOr6-imSHAIlSphGj6G4rUdbMtBV0n1MVGg3vIyHQot3hMaH6uXMpHOUEtxQivkp0F-fY6PoFr49HfWD-ZuneENaKKjB8p_rd9k";
    const EXPIRED_TOKEN: &str = "eyJhbGciOiJSUzUxMiJ9.eyJpc3MiOiJ0YWJieW1sLmNvbSIsInN1YiI6ImZha2VAdGFiYnltbC5jb20iLCJpYXQiOjE3MDUxOTgxMDIsImV4cCI6MTcwNDM5ODcwMiwidHlwIjoiVEVBTSIsIm51bSI6MX0.UBufd2YlyhuChdCSZvbvEBtxLABhZSuhya4KHKHYM2ABaSTjYYtSyT-yv0i9b8sySBoeu7kG0XBNrLQOg4fcirR5DxOFxiskI7qLLSQEIDYe-xnEbvxqKhN3RpHkxik9_OlvElvpIGrZRQxiELhESIM0NGck0Dz6MwTDFutkHZFh06cLFeoihs1rn44SknL3wP_afyCaOpQtTjDfsayBMfyDAriTG8HSnPbrw5Om7ER7uAqszhX8wpFonDeFeVB0OIUjayfL-SAMdLqNEqaFsUcuE4cUk7o9tA2jsYz2-BRlwDocLpRVp2V-K8MuyQJhDTiswbey2DE5tNRvnd3nNaVr7Pmt3mF7NMt8op8hl4I9scoThFBj9Bb1iMfAGVSXlRn9Kf2HHe2BJXGWC3w9bjWH2KRPMP3tScJ4CQccIJxZPU-fcX7IC1q8R4PWDYS11TDJ03PvCTEGFt3fBTLLaGOeoYHYNnd4qux317YhGtWTOO6ESIuoxQkJdTpNVOwfNmCVSfFUvJYs0l4r7z-QouHAd79Ck_GJ-cdiIOrV9MB1Lq6ayk267bXfdi0Lx6-PYxrTwXEkF5tBydrsPyhoReAbH8yQDqzlPbQzOlLo--Z4940kSEpgEsL9G6ymG5wDlMzNuQfjbYbCI0L19Spx5QRGtyYXtiSU1Tq-hhGm3zA";
    const INCOMPLETE_TOKEN: &str = "eyJhbGciOiJSUzUxMiJ9.eyJpc3MiOiJ0YWJieW1sLmNvbSIsInN1YiI6ImZha2VAdGFiYnltbC5jb20iLCJpYXQiOjE3MDUxOTgxMDIsImV4cCI6MTgwNDM5ODcwMiwidHlwIjoiVEVBTSJ9.juNQeg8jMRj7Q2XbmHSdneKZbTP_BIL43yW3He5avIRAKee1NF9-qg4ndGOYVWBmtoO6Y_CAts_trSw6gmuDuwWcmSbbr7CWQOYuNrMj1_Gp1MctA8zzC3yzr0EoBLzqkNBq3OySlfOkohopmJ6Lu0d0KRtf46qq94cMDAlfs7etcVGkGqfMEwxznptXiF7_S3qRVbahvJDPJlu_ozwn51tICXMrlGV_P6jdBcNLQ8I1LAH2RfyH9u-4mUSTKt-obnXw6mtPxPjl07MEajM_wW3X05-iRygQfyzDulvW0EXf39OnW2kCuyfQWx5Zksr-sCNTEL2VSalf9o8MchjAhDN5QrygdZkk7KXwt3O54tpcnFVABw9ORxJtTrsZJD-YvdmS01O6qLfMRWs2CGWFTfDJLxMSiBhAsy4DC4TkZN4UnBpX09U7n6f_0NUr83YAWcw0Rlp32k01j9iPUWSdePZh46Ck00XdzLcc15xfqv__ilaLAyRtb9JUVBX7g-VaLb1YGk658t19eukRNzE6WFyKfAE7u6EbxowtFQqVKYXWX_zDHoalo3DjUmPBV_VsorcBg4cjhrhBPBOB5f7Wa8r7eiJz1gWEj1xJEK2Y_mdShAvxNSWPSTvNvviPTgJbvbwDTzQ0It_d066ADBY2o0y5DTMP23EPL-oZ14TYIY4";

    #[test]
    fn test_validate_license() {
        let license = validate_license(VALID_TOKEN).unwrap();
        assert_eq!(license.iss, "tabbyml.com");
        assert_eq!(license.sub, "fake@tabbyml.com");
        assert_matches!(license.typ, LicenseType::Team);
        assert_eq!(
            license_info_from_raw(license, 11).unwrap().status,
            LicenseStatus::SeatsExceeded
        );
    }

    #[test]
    fn test_expired_license() {
        let license = validate_license(EXPIRED_TOKEN).unwrap();
        let license = license_info_from_raw(license, 0).unwrap();
        assert_matches!(license.status, LicenseStatus::Expired);
    }

    #[test]
    fn test_missing_field() {
        let license = validate_license(INCOMPLETE_TOKEN);
        assert_matches!(
            license,
            Err(jwt::errors::ErrorKind::MissingRequiredClaim(_))
        );
    }

    #[tokio::test]
    async fn test_create_update_license() {
        let db = DbConn::new_in_memory().await.unwrap();
        let service = new_license_service(db).await.unwrap();

        assert!(service.update_license("bad_token".into()).await.is_err());

        service.update_license(VALID_TOKEN.into()).await.unwrap();
        assert!(service.read_license().await.unwrap().is_some());

        assert!(service.update_license(EXPIRED_TOKEN.into()).await.is_err());
    }
}
