// SPDX-License-Identifier: BUSL-1.1
//! Compile KEDA's `externalscaler.proto` into the tonic gRPC server stubs.
//!
//! tonic 0.14 split prost codegen out of `tonic-build` into `tonic-prost-build`
//! (it bundles `tonic-build` + `prost-build`); the generated code is emitted into
//! `OUT_DIR` and pulled in via `tonic::include_proto!("externalscaler")` from
//! `src/pb.rs`. The system `protoc` (/usr/bin/protoc) drives the parse. We build
//! the SERVER only — agentctl serves this contract, it never calls it.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .build_client(false)
        .compile_protos(&["proto/externalscaler.proto"], &["proto"])?;
    Ok(())
}
