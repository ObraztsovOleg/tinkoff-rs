//! Сгенерированные protobuf-сообщения и gRPC clients T-Invest.
//!
//! Модуль собирается из файлов `contracts/*.proto` скриптом `build.rs`. Raw API
//! использует эти типы напрямую, а `broker` преобразует самые частые ответы в
//! более узкие типы модуля `domain`.

#![allow(clippy::all, clippy::pedantic)]

tonic::include_proto!("tinkoff.public.invest.api.contract.v1");
