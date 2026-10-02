fn main() -> Result<(), Box<dyn std::error::Error>> {
    // types.proto (shared types), events.proto (input events), and session.proto
    // (HudSession service, package tze_hud.protocol.v1.session).
    tonic_build::configure().compile_protos(
        &[
            "proto/types.proto",
            "proto/events.proto",
            "proto/session.proto",
        ],
        &["proto"],
    )?;
    Ok(())
}
