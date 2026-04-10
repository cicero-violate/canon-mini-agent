use anyhow::{Context, Result};

fn main() -> Result<()> {
    let workspace = std::env::current_dir().context("get current dir")?;

    let idx = canon_mini_agent::SemanticIndex::load(&workspace, "canon_mini_agent").context(
        "load semantic index (expected state/rustc/canon_mini_agent/graph.json). Run `cargo build` or `cargo test` first if missing.",
    )?;

    let old_symbol = "rename_example_target::example_target_fn";
    let new_symbol = "rename_example_target::example_target_fn_renamed";

    let report = canon_mini_agent::rename_semantic::rename_symbol_via_semantic_spans(
        &workspace,
        &idx,
        old_symbol,
        new_symbol,
    )
    .with_context(|| format!("rename {old_symbol} -> {new_symbol}"))?;

    println!(
        "rename ok: replacements={} touched_files={}",
        report.replacements,
        report.touched_files.len()
    );
    for path in &report.touched_files {
        println!("  {}", path.display());
    }

    Ok(())
}

