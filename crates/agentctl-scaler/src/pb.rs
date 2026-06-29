// SPDX-License-Identifier: BUSL-1.1
//! The tonic/prost types generated from `proto/externalscaler.proto` (KEDA's
//! external scaler contract, package `externalscaler`). `build.rs` emits the code
//! into `OUT_DIR`; this pulls it in under the `externalscaler` module path so the
//! rest of the crate refers to `pb::IsActiveResponse`, `pb::MetricSpec`, … and
//! `pb::external_scaler_server::{ExternalScaler, ExternalScalerServer}`.
#![allow(clippy::doc_overindented_list_items)]

tonic::include_proto!("externalscaler");
