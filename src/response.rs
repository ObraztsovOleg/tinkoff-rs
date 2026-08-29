//! Ответ API вместе со служебными заголовками T-Invest.
//!
//! gRPC payload и metadata живут в разных частях `tonic::Response`. Этот модуль
//! объединяет их, чтобы прикладной код не потерял tracking id и rate-limit.

use tonic::{Response, metadata::MetadataMap};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
/// Снимок ограничений частоты запросов из response metadata.
pub struct RateLimit {
    /// Разрешённое число запросов в текущем окне, если сервер прислал заголовок.
    pub limit: Option<u64>,
    /// Оставшееся число запросов в текущем окне.
    pub remaining: Option<u64>,
    /// Число секунд до сброса ограничения.
    pub reset_seconds: Option<u64>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
/// Служебные данные, сопровождающие полезный результат запроса.
pub struct ApiMetadata {
    /// Идентификатор запроса для диагностики вместе с поддержкой T-Invest.
    pub tracking_id: Option<String>,
    /// Доступная информация о rate-limit.
    pub rate_limit: RateLimit,
}

impl ApiMetadata {
    /// Безопасно извлекает известные заголовки; отсутствующие или некорректные
    /// значения превращаются в `None`, а не делают весь запрос ошибочным.
    #[must_use]
    pub fn from_headers(headers: &MetadataMap) -> Self {
        Self {
            tracking_id: header_string(headers, "x-tracking-id"),
            rate_limit: RateLimit {
                limit: header_u64(headers, "x-ratelimit-limit"),
                remaining: header_u64(headers, "x-ratelimit-remaining"),
                reset_seconds: header_u64(headers, "x-ratelimit-reset"),
            },
        }
    }
}

#[derive(Clone, Debug)]
/// Полезные данные `data` и metadata одного unary-ответа.
pub struct ApiResponse<T> {
    /// Доменный или raw protobuf результат запроса.
    pub data: T,
    /// Tracking id и информация об ограничениях API.
    pub metadata: ApiMetadata,
}

impl<T> ApiResponse<T> {
    /// Разделяет `tonic::Response<T>` на payload и metadata.
    #[must_use]
    pub fn from_tonic(response: Response<T>) -> Self {
        let metadata = ApiMetadata::from_headers(response.metadata());
        Self {
            data: response.into_inner(),
            metadata,
        }
    }

    /// Возвращает только payload, если metadata вызывающему коду не нужна.
    #[must_use]
    pub fn into_inner(self) -> T {
        self.data
    }

    /// Преобразует payload, сохраняя metadata исходного запроса.
    #[must_use]
    pub fn map<U>(self, map: impl FnOnce(T) -> U) -> ApiResponse<U> {
        ApiResponse {
            data: map(self.data),
            metadata: self.metadata,
        }
    }
}

fn header_string(headers: &MetadataMap, name: &'static str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

fn header_u64(headers: &MetadataMap, name: &'static str) -> Option<u64> {
    header_string(headers, name).and_then(|value| value.parse().ok())
}
