# @agentkit/native-linux-x64-gnu

Prebuilt Linux x64 glibc addon for `@agentkit/native`, selected automatically
as an `optionalDependency` (os/cpu/libc gating). The release build stages the
compiled addon before packing; the `prepack` script refuses to pack without
it. Installation never builds Rust code.
