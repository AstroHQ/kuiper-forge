fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        // boxed so the command enums stay small (clippy::large_enum_variant)
        .boxed(".cirunner.agent.CreateRunnerCommand.forgejo")
        .compile_protos(&["proto/agent.proto"], &["proto"])?;
    tonic_prost_build::compile_protos("proto/management.proto")?;
    Ok(())
}
