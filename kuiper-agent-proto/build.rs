fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::compile_protos("proto/agent.proto")?;
    tonic_prost_build::compile_protos("proto/management.proto")?;
    Ok(())
}
