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
            trace_error(
                "invalid_uid",
                "Не удалось определить игровой сервер по UID.",
            );
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

fn trace_error(kind: &str, message: &str) {
    tracing::Span::current()
        .record("error.type", kind)
        .record("error.message", message);
}

fn trace_transport_error(error: &reqwest::Error) {
    let (kind, message) = if error.is_timeout() {
        ("timeout", "Истекло время ожидания ответа HoYoLAB.")
    } else if error.is_connect() {
        ("connection", "Не удалось установить соединение с HoYoLAB.")
    } else if error.is_body() || error.is_decode() {
        ("response_body", "Не удалось прочитать тело ответа HoYoLAB.")
    } else {
        ("transport", "Не удалось выполнить HTTP-запрос к HoYoLAB.")
    };
    trace_error(kind, message);
}

fn decode_json<T>(result: serde_json::Result<T>) -> Result<T, HoyoverseError> {
    result.map_err(|error| {
        let detail = error.to_string();
        // Serde's missing-field diagnostic names a field of the destination struct.
        // Other data errors can quote an entire value from the private response.
        let missing = detail
            .strip_prefix("missing field `")
            .and_then(|rest| rest.split_once('`'))
            .map(|(field, _)| field)
            .filter(|name| {
                !name.is_empty()
                    && name.len() <= 64
                    && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            });
        let reason = match error.classify() {
            serde_json::error::Category::Eof => "Ответ HoYoLAB оборвался до конца JSON",
            serde_json::error::Category::Syntax => "HoYoLAB вернул некорректный JSON",
            serde_json::error::Category::Data => {
                "JSON HoYoLAB не соответствует ожидаемой структуре"
            }
            serde_json::error::Category::Io => "Не удалось прочитать JSON HoYoLAB",
        };
        if let Some(field) = missing {
            trace_error(
                "decode",
                &format!("В ответе HoYoLAB отсутствует обязательное поле {field}."),
            );
        } else if error.line() > 0 {
            trace_error(
                "decode",
                &format!(
                    "{reason} (строка {}, столбец {}).",
                    error.line(),
                    error.column()
                ),
            );
        } else {
            trace_error("decode", reason);
        }
        HoyoverseError::Deserialize(detail)
    })
}

fn decode_response<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, HoyoverseError> {
    // Read the API status before interpreting the endpoint-specific data schema.
    let response: ApiResponse<Box<serde_json::value::RawValue>> =
        decode_json(serde_json::from_str(text))?;
    tracing::Span::current().record("hoyolab.retcode", response.retcode);
    match response.retcode {
        0 => match response.data {
            Some(data) => decode_json(serde_json::from_str(data.get())),
            None => {
                trace_error(
                    "no_data",
                    "HoYoLAB вернул успешный ответ без данных игрока.",
                );
                Err(HoyoverseError::DataNotPublic)
            }
        },
        -101 | 10001 => {
            trace_error(
                "auth",
                "HoYoLAB отклонил cookie: авторизация недействительна или истекла.",
            );
            Err(HoyoverseError::Auth)
        }
        -501000 => {
            trace_error(
                "data_not_public",
                "Игрок закрыл доступ к статистике HoYoLAB.",
            );
            Err(HoyoverseError::DataNotPublic)
        }
        code => {
            trace_error("api", &format!("HoYoLAB отклонил запрос (retcode {code})."));
            Err(HoyoverseError::Api {
                retcode: code,
                message: response.message,
            })
        }
    }
}

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

    async fn request<T>(&self, request: reqwest::RequestBuilder) -> Result<T, HoyoverseError>
    where
        T: serde::de::DeserializeOwned,
    {
        let resp = request.send().await.inspect_err(trace_transport_error)?;
        let status = resp.status();
        tracing::Span::current().record("http.response.status_code", i64::from(status.as_u16()));
        let text = resp.text().await.inspect_err(trace_transport_error)?;

        if !status.is_success() {
            trace_error("http_status", &format!("HoYoLAB вернул HTTP {status}."));
            return Err(HoyoverseError::Other(format!("HTTP {status}: {text}")));
        }

        decode_response(&text)
    }

    // ── Public API ──

    /// Get the list of game records (linked games) for the HoYoLAB account.
    #[tracing::instrument(
        target = "rustverse::bot",
        name = "hoyolab.game_record_cards",
        skip_all,
        fields(error.type, error.message, http.response.status_code, hoyolab.retcode), err
    )]
    pub async fn get_game_record_cards(
        &self,
    ) -> Result<Vec<crate::models::common::GameRecordCard>, HoyoverseError> {
        let request = self
            .client
            .get(format!(
                "{CARD_URL}/event/game_record/card/wapi/getGameRecordCard"
            ))
            .headers(self.headers_no_ds())
            .query(&[("uid", &self.ltuid)]);

        let data: GameRecordCardResponse = self.request(request).await?;
        Ok(data.list)
    }

    /// Get the ZZZ Daily Note. Detect the server from the UID when it is `None`.
    #[tracing::instrument(target = "rustverse::bot", name = "hoyolab.daily_note", skip_all, fields(error.type, error.message, http.response.status_code, hoyolab.retcode), err)]
    pub async fn get_daily_note(
        &self,
        role_id: &str,
        server: Option<&str>,
    ) -> Result<ZZZDailyNote, HoyoverseError> {
        let server = resolve_server(role_id, server)?;
        let request = self
            .client
            .get(format!("{ZZZ_BASE_URL}/event/game_record_zzz/api/zzz/note"))
            .headers(self.headers_with_ds())
            .query(&[("role_id", role_id), ("server", server)]);

        self.request(request).await
    }

    /// Get Shiyu Defense data. Detect the server from the UID when it is `None`.
    #[tracing::instrument(
        target = "rustverse::bot",
        name = "hoyolab.shiyu_defense",
        skip_all,
        fields(error.type, error.message, http.response.status_code, hoyolab.retcode), err
    )]
    pub async fn get_shiyu_defense(
        &self,
        role_id: &str,
        server: Option<&str>,
        schedule_type: &str,
    ) -> Result<ZZZShiyuDefense, HoyoverseError> {
        let server = resolve_server(role_id, server)?;
        let request = self
            .client
            .get(format!(
                "{ZZZ_BASE_URL}/event/game_record_zzz/api/zzz/hadal_info_v2"
            ))
            .headers(self.headers_with_ds())
            .query(&[
                ("role_id", role_id),
                ("server", server),
                ("schedule_type", schedule_type),
            ]);

        // The data is wrapped in a versioned key: hadal_info_v1 or hadal_info_v2.
        let data: serde_json::Value = self.request(request).await?;

        if let Some(inner) = data
            .get("hadal_info_v2")
            .or_else(|| data.get("hadal_info_v1"))
        {
            decode_json(serde_json::from_value(inner.clone()))
        } else {
            trace_error(
                "missing_shiyu_data",
                "В ответе HoYoLAB отсутствуют поля hadal_info_v2 и hadal_info_v1.",
            );
            Err(HoyoverseError::Deserialize(
                "missing hadal_info_v1/v2 key".into(),
            ))
        }
    }

    /// Get Deadly Assault data for a `uid` and `region`.
    /// Detect the server from the UID when it is `None`.
    #[tracing::instrument(
        target = "rustverse::bot",
        name = "hoyolab.deadly_assault",
        skip_all,
        fields(error.type, error.message, http.response.status_code, hoyolab.retcode), err
    )]
    pub async fn get_deadly_assault(
        &self,
        uid: &str,
        region: Option<&str>,
        schedule_type: &str,
    ) -> Result<ZZZDeadlyAssault, HoyoverseError> {
        let region = resolve_server(uid, region)?;
        let request = self
            .client
            .get(format!(
                "{ZZZ_BASE_URL}/event/game_record_zzz/api/zzz/hadal_mem_detail_v2"
            ))
            .headers(self.headers_with_ds())
            .query(&[
                ("uid", uid),
                ("region", region),
                ("schedule_type", schedule_type),
            ]);

        self.request(request).await
    }

    /// Get the gacha (banner) calendar. Uses `uid`/`region`. Server auto-detected if None.
    #[tracing::instrument(
        target = "rustverse::bot",
        name = "hoyolab.gacha_calendar",
        skip_all,
        fields(error.type, error.message, http.response.status_code, hoyolab.retcode), err
    )]
    pub async fn get_gacha_calendar(
        &self,
        uid: &str,
        region: Option<&str>,
    ) -> Result<ZZZGachaCalendar, HoyoverseError> {
        let region = resolve_server(uid, region)?;
        let request = self
            .client
            .get(format!(
                "{ZZZ_BASE_URL}/event/game_record_zzz/api/zzz/gacha_calendar"
            ))
            .headers(self.headers_with_ds())
            .query(&[("uid", uid), ("region", region)]);

        self.request(request).await
    }

    /// Get the ZZZ game record index (profile summary + stats + avatar list).
    /// Use the public API domain for public third-party UID data.
    /// Detect the server from the UID when it is `None`.
    #[tracing::instrument(target = "rustverse::bot", name = "hoyolab.index", skip_all, fields(error.type, error.message, http.response.status_code, hoyolab.retcode), err)]
    pub async fn get_index(
        &self,
        role_id: &str,
        server: Option<&str>,
    ) -> Result<ZZZIndex, HoyoverseError> {
        let server = resolve_server(role_id, server)?;
        let request = self
            .client
            .get(format!(
                "{ZZZ_AVATAR_URL}/event/game_record_zzz/api/zzz/index"
            ))
            .headers(self.headers_with_ds())
            .query(&[("role_id", role_id), ("server", server)]);

        let data: ZZZIndex = self.request(request).await?;
        agent_cache::cache_avatars(&data.avatar_list);
        Ok(data)
    }

    /// Get the player agent list.
    /// Detect the server from the UID when it is `None`.
    #[tracing::instrument(target = "rustverse::bot", name = "hoyolab.avatar_list", skip_all, fields(error.type, error.message, http.response.status_code, hoyolab.retcode), err)]
    pub async fn get_avatar_list(
        &self,
        role_id: &str,
        server: Option<&str>,
    ) -> Result<ZZZAvatarList, HoyoverseError> {
        let server = resolve_server(role_id, server)?;
        let request = self
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
            ]);

        let data: ZZZAvatarList = self.request(request).await?;
        agent_cache::cache_avatars(&data.avatar_list);
        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_responses_do_not_require_a_successful_payload_shape() {
        for (retcode, expected) in [
            (-101, "auth"),
            (10001, "auth"),
            (-501000, "private"),
            (42, "api"),
        ] {
            let body = serde_json::json!({"retcode":retcode, "message":"private upstream response", "data":{"unexpected":true}});
            let error = decode_response::<Vec<String>>(&body.to_string()).unwrap_err();
            assert!(match error {
                HoyoverseError::Auth => expected == "auth",
                HoyoverseError::DataNotPublic => expected == "private",
                HoyoverseError::Api { retcode: code, .. } => expected == "api" && code == retcode,
                _ => false,
            });
        }
        assert_eq!(
            decode_response::<Vec<i64>>(r#"{"retcode":0,"message":"OK","data":[1]}"#).unwrap(),
            vec![1]
        );
        assert!(matches!(
            decode_response::<Vec<i64>>(r#"{"retcode":0,"message":"OK","data":null}"#),
            Err(HoyoverseError::DataNotPublic)
        ));
    }
}
