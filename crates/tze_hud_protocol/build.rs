fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Select the pinned build-host compiler, not PATH or a local PROTOC override.
    let protoc = protoc_bin_vendored::protoc_bin_path().map_err(|error| {
        format!("cannot select bundled protoc 31.1 for this build host: {error}")
    })?;
    let mut config = prost_build::Config::new();
    config.protoc_executable(protoc);

    // types.proto (shared types), events.proto (input events), and session.proto
    // (HudSession service, package tze_hud.protocol.v1.session).
    tonic_build::configure().compile_protos_with_config(
        config,
        &[
            "proto/types.proto",
            "proto/events.proto",
            "proto/session.proto",
        ],
        &["proto"],
    )?;
    Ok(())
}
