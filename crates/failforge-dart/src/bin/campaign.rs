//! Installed transport helper; examples and CLI share the same implementation.
#[path = "../../examples/campaign.rs"]
mod campaign;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    campaign::main()
}
