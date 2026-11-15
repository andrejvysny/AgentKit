//! The language-server tools as the `code_intel` toolset (T4.1, ADR 0033).

use agentkit_core::Toolset;

pub const CODE_INTEL_TOOLSET: &str = "code_intel";

pub fn lsp_toolsets() -> Vec<Toolset> {
    vec![
        Toolset::new(
            CODE_INTEL_TOOLSET,
            "Code intelligence",
            "Definitions, references, hover, symbols and diagnostics from the language server.",
        )
        .with_guide(include_str!("../guides/code_intel.md"))
        .with_tools(&[
            "lsp_definition",
            "lsp_references",
            "lsp_hover",
            "lsp_symbols",
            "lsp_implementation",
            "lsp_diagnostics",
        ]),
    ]
}
