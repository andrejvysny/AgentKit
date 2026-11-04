# @agentkit/native-darwin-x64

Prebuilt macOS x64 addon for `@agentkit/native`, selected automatically as an
`optionalDependency` (os/cpu gating). The release build cross-compiles the
addon and stages it before packing; the `prepack` script refuses to pack
without it. Installation never builds Rust code.
