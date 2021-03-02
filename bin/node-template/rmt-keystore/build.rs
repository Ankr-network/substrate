fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::compile_protos("/Users/felipe/dev/ankr/stkr-proto-contract/v2/blockchain-signer.proto")?;
    Ok(())
}