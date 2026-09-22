//! Prints the controller API's OpenAPI spec — the input to the SPA's TypeScript codegen
//! (`npm run generate`). A dedicated bin so the codegen compiles only this crate, not the
//! whole `crucible` binary tree, and needs no ui/dist (build.rs creates it empty).
fn main() -> anyhow::Result<()> {
    println!("{}", crucible_controller::openapi_spec()?);
    Ok(())
}
