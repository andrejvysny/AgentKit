# @agentkit/native-darwin-arm64

Prebuilt macOS arm64 addon for `@agentkit/native`, selected automatically as
an `optionalDependency` (os/cpu gating). The release build stages the
compiled addon before packing; the `prepack` script refuses to pack without
it. Installation never builds Rust code.
