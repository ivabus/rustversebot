use reqwest::Client;

use super::agent_cache;
use super::ds;
use super::server;
use crate::error::HoyoverseError;
use crate::models::common::{ApiResponse, GameRecordCardResponse};
use crate::models::zzz::*;

/// Resolve server from explicit value or auto-detect from UID.
fn resolve_server<'a>(uid: &str, explicit: Option<&'a str>) -> Result<&'a str, HoyoverseError> {
    match explicit {
        Some(s) if !s.is_empty() => Ok(s),
        _ => server::detect_server(uid).ok_or_else(|| {
            HoyoverseError::Other(format!(
                "Cannot detect server from UID '{uid}'. Use --server explicitly."
            ))
        }),
    }
}

const ZZZ_BASE_URL: &str = "https://sg-act-nap-api.hoyolab.com";
const ZZZ_AVATAR_URL: &str = "https://sg-act-public-api.hoyolab.com";
const CARD_URL: &str = "https://sg-public-api.hoyolab.com";

const UA_STRING: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

pub struct ZZZClient {
    client: Client,
    cookies: String,
    #[allow(dead_code)]
    ltoken: String,
    #[allow(dead_code)]
    ltuid: String,
    #[allow(dead_code)]
    ltmid: String,
}

impl ZZZClient {
    /// Create a new ZZZ client from raw cookie values.
    ///
    /// # Arguments
    /// * `ltuid_v2` — HoYoLAB account UID
    /// * `ltoken_v2` — authentication token
    /// * `ltmid_v2` — MID token
    pub fn new(
        ltuid_v2: impl Into<String>,
        ltoken_v2: impl Into<String>,
        ltmid_v2: impl Into<String>,
    ) -> Self {
        let ltuid = ltuid_v2.into();
        let ltoken = ltoken_v2.into();
        let ltmid = ltmid_v2.into();

        let cookies = format!(
            "ltoken_v2={ltoken}; ltuid_v2={ltuid}; ltmid_v2={ltmid}",
            ltoken = ltoken,
            ltuid = ltuid,
            ltmid = ltmid,
        );

        Self {
            client: Client::new(),
            cookies,
            ltoken,
            ltuid,
            ltmid,
        }
    }

    /// Create a client from a raw Cookie header string.
    /// The string should contain `ltoken_v2=...; ltuid_v2=...; ltmid_v2=...`.
    pub fn from_cookie_string(cookie_header: &str) -> Result<Self, HoyoverseError> {
        fn extract(cookie: &str, key: &str) -> Option<String> {
            cookie
                .split(';')
                .map(|s| s.trim())
                .find(|s| s.starts_with(&format!("{key}=")))
                .and_then(|s| s.split('=').nth(1))
                .map(|v| v.to_string())
        }

        let ltuid = extract(cookie_header, "ltuid_v2")
            .ok_or_else(|| HoyoverseError::Other("ltuid_v2 not found in cookie".into()))?;
        let ltoken = extract(cookie_header, "ltoken_v2")
            .ok_or_else(|| HoyoverseError::Other("ltoken_v2 not found in cookie".into()))?;
        let ltmid = extract(cookie_header, "ltmid_v2")
            .ok_or_else(|| HoyoverseError::Other("ltmid_v2 not found in cookie".into()))?;

        Ok(Self::new(ltuid, ltoken, ltmid))
    }

    /// Build common headers for requests that need DS.
    fn headers_with_ds(&self) -> reqwest::header::HeaderMap {
        use reqwest::header::{
            ACCEPT, ACCEPT_LANGUAGE, COOKIE, HeaderMap, HeaderValue, ORIGIN, REFERER, USER_AGENT,
        };

        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(UA_STRING));
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/plain, */*"),
        );
        headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.9"));
        headers.insert("x-rpc-app_version", HeaderValue::from_static("1.5.0"));
        headers.insert("x-rpc-client_type", HeaderValue::from_static("5"));
        headers.insert("x-rpc-language", HeaderValue::from_static("en-us"));
        headers.insert(ORIGIN, HeaderValue::from_static("https://act.hoyolab.com"));
        headers.insert(
            REFERER,
            HeaderValue::from_static("https://act.hoyolab.com/"),
        );
        headers.insert(COOKIE, HeaderValue::from_str(&self.cookies).unwrap());
        headers.insert("DS", HeaderValue::from_str(&ds::generate_ds()).unwrap());
        headers
    }

    /// Build headers without DS for endpoints that do not require it.
    fn headers_no_ds(&self) -> reqwest::header::HeaderMap {
        let mut headers = self.headers_with_ds();
        headers.remove("DS");
        headers
    }

    // ── Helpers ──

    async fn check_response<T>(&self, resp: reqwest::Response) -> Result<T, HoyoverseError>
    where
        T: serde::de::DeserializeOwned,
    {
        let status = resp.status();
        let text = resp.text().await?;

        if !status.is_success() {
            return Err(HoyoverseError::Other(format!("HTTP {status}: {text}")));
        }

        let api_resp: ApiResponse<T> = serde_json::from_str(&text)?;
        #[cfg(debug_assertions)]
        {
            let _value: serde_json::Value = serde_json::from_str(&text)?;
            println!("{:#?}", _value);
        }
        match api_resp.retcode {
            0 => api_resp.data.ok_or(HoyoverseError::DataNotPublic),
            -101 | 10001 => Err(HoyoverseError::Auth),
            -501000 => Err(HoyoverseError::DataNotPublic),
            _ => Err(HoyoverseError::Api {
                retcode: api_resp.retcode,
                message: api_resp.message,
            }),
        }
    }

    // ── Public API ──

    /// Get the list of game records (linked games) for the HoYoLAB account.
    pub async fn get_game_record_cards(
        &self,
    ) -> Result<Vec<crate::models::common::GameRecordCard>, HoyoverseError> {
        let resp = self
            .client
            .get(format!(
                "{CARD_URL}/event/game_record/card/wapi/getGameRecordCard"
            ))
            .headers(self.headers_no_ds())
            .query(&[("uid", &self.ltuid)])
            .send()
            .await?;

        let data: GameRecordCardResponse = self.check_response(resp).await?;
        Ok(data.list)
    }

    /// Get the ZZZ Daily Note. Detect the server from the UID when it is `None`.
    pub async fn get_daily_note(
        &self,
        role_id: &str,
        server: Option<&str>,
    ) -> Result<ZZZDailyNote, HoyoverseError> {
        let server = resolve_server(role_id, server)?;
        let resp = self
            .client
            .get(format!("{ZZZ_BASE_URL}/event/game_record_zzz/api/zzz/note"))
            .headers(self.headers_with_ds())
            .query(&[("role_id", role_id), ("server", server)])
            .send()
            .await?;

        self.check_response(resp).await
    }

    /// Get Shiyu Defense data. Detect the server from the UID when it is `None`.
    pub async fn get_shiyu_defense(
        &self,
        role_id: &str,
        server: Option<&str>,
        schedule_type: &str,
    ) -> Result<ZZZShiyuDefense, HoyoverseError> {
        let server = resolve_server(role_id, server)?;
        let resp = self
            .client
            .get(format!(
                "{ZZZ_BASE_URL}/event/game_record_zzz/api/zzz/hadal_info_v2"
            ))
            .headers(self.headers_with_ds())
            .query(&[
                ("role_id", role_id),
                ("server", server),
                ("schedule_type", schedule_type),
            ])
            .send()
            .await?;

        // The data is wrapped in a versioned key: hadal_info_v1 or hadal_info_v2.
        let data: serde_json::Value = {
            let status = resp.status();
            let text = resp.text().await?;

            if !status.is_success() {
                return Err(HoyoverseError::Other(format!("HTTP {status}: {text}")));
            }

            let api_resp: ApiResponse<serde_json::Value> = serde_json::from_str(&text)?;
            match api_resp.retcode {
                0 => api_resp.data.ok_or(HoyoverseError::DataNotPublic)?,
                -101 | 10001 => return Err(HoyoverseError::Auth),
                -501000 => return Err(HoyoverseError::DataNotPublic),
                _ => {
                    return Err(HoyoverseError::Api {
                        retcode: api_resp.retcode,
                        message: api_resp.message,
                    });
                }
            }
        };

        if let Some(inner) = data.get("hadal_info_v2") {
            Ok(serde_json::from_value(inner.clone())?)
        } else if let Some(inner) = data.get("hadal_info_v1") {
            Ok(serde_json::from_value(inner.clone())?)
        } else {
            Err(HoyoverseError::Deserialize(
                "missing hadal_info_v1/v2 key".into(),
            ))
        }
    }

    /// Get Deadly Assault data for a `uid` and `region`.
    /// Detect the server from the UID when it is `None`.
    pub async fn get_deadly_assault(
        &self,
        uid: &str,
        region: Option<&str>,
        schedule_type: &str,
    ) -> Result<ZZZDeadlyAssault, HoyoverseError> {
        let region = resolve_server(uid, region)?;
        let resp = self
            .client
            .get(format!(
                "{ZZZ_BASE_URL}/event/game_record_zzz/api/zzz/hadal_mem_detail_v2"
            ))
            .headers(self.headers_with_ds())
            .query(&[
                ("uid", uid),
                ("region", region),
                ("schedule_type", schedule_type),
            ])
            .send()
            .await?;

        self.check_response(resp).await
    }

    /// Get the gacha (banner) calendar. Uses `uid`/`region`. Server auto-detected if None.
    pub async fn get_gacha_calendar(
        &self,
        uid: &str,
        region: Option<&str>,
    ) -> Result<ZZZGachaCalendar, HoyoverseError> {
        let region = resolve_server(uid, region)?;
        let resp = self
            .client
            .get(format!(
                "{ZZZ_BASE_URL}/event/game_record_zzz/api/zzz/gacha_calendar"
            ))
            .headers(self.headers_with_ds())
            .query(&[("uid", uid), ("region", region)])
            .send()
            .await?;

        self.check_response(resp).await
    }

    /// Get the ZZZ game record index (profile summary + stats + avatar list).
    /// Use the public API domain for public third-party UID data.
    /// Detect the server from the UID when it is `None`.
    pub async fn get_index(
        &self,
        role_id: &str,
        server: Option<&str>,
    ) -> Result<ZZZIndex, HoyoverseError> {
        let server = resolve_server(role_id, server)?;
        let resp = self
            .client
            .get(format!(
                "{ZZZ_AVATAR_URL}/event/game_record_zzz/api/zzz/index"
            ))
            .headers(self.headers_with_ds())
            .query(&[("role_id", role_id), ("server", server)])
            .send()
            .await?;

        let data: ZZZIndex = self.check_response(resp).await?;
        agent_cache::cache_avatars(&data.avatar_list);
        Ok(data)
    }

    /// Get the player agent list.
    /// Detect the server from the UID when it is `None`.
    pub async fn get_avatar_list(
        &self,
        role_id: &str,
        server: Option<&str>,
    ) -> Result<ZZZAvatarList, HoyoverseError> {
        let server = resolve_server(role_id, server)?;
        let resp = self
            .client
            .get(format!(
                "{ZZZ_AVATAR_URL}/event/game_record_zzz/api/zzz/avatar/basic"
            ))
            .headers(self.headers_with_ds())
            .query(&[
                ("role_id", role_id),
                ("server", server),
                ("uid", role_id),
                ("region", server),
            ])
            .send()
            .await?;

        let data: ZZZAvatarList = self.check_response(resp).await?;
        agent_cache::cache_avatars(&data.avatar_list);
        Ok(data)
    }
}
