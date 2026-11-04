# @agentkit/native-win32-x64-msvc

Prebuilt Windows x64 MSVC addon for `@agentkit/native`, selected
automatically as an `optionalDependency` (os/cpu gating). The release build
stages the compiled addon before packing; the `prepack` script refuses to
pack without it. Installation never builds Rust code.
